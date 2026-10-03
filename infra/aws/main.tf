# An install of torpor: the app and native-code buckets, the serving and compile functions, CloudFront in front of
# the serving function at *.<domain>, the teams' roles, and a budget alert.

data "aws_caller_identity" "current" {}

data "aws_route53_zone" "this" {
  name = var.domain
}

locals {
  account = data.aws_caller_identity.current.account_id
  app     = aws_s3_bucket.this["app"].arn
  native  = aws_s3_bucket.this["native"].arn
  # The Lambda Web Adapter, published by AWS: an extension that turns invocations into HTTP requests to the function.
  adapter = "arn:aws:lambda:${var.region}:753240598075:layer:LambdaAdapterLayerArm64:30"
  zip     = "../../dist/torpor.zip"     # as `docker compose run --rm release` builds it
  team    = "$${aws:PrincipalTag/team}" # for IAM to fill in: the `team` tag of the role
  env = {
    TORPOR_STORE                     = "s3://${aws_s3_bucket.this["app"].bucket}"
    TORPOR_NATIVE                    = "s3://${aws_s3_bucket.this["native"].bucket}"
    RUST_LOG                         = var.logs.filter
    AWS_LWA_READINESS_CHECK_PROTOCOL = "tcp" # as an HTTP check would cost `serve` a bucket read
  }
  # What each function may do, besides write its logs.
  grants = {
    serve = [
      { Action = ["s3:GetObject"], Resource = ["${local.app}/apps/*", "${local.native}/*"] },
      { Action = ["s3:GetObject", "s3:PutObject", "s3:DeleteObject"], Resource = ["${local.app}/kv/*"] },
      { Action = ["s3:PutObject"], Resource = ["${local.app}/compile/*"] },
      { Action = ["s3:ListBucket"], Resource = [local.app, local.native] }, # so a missing key is a 404, not a 403
    ]
    compile = [
      { Action = ["s3:GetObject"], Resource = ["${local.app}/apps/*/blobs/*"] },
      { Action = ["s3:DeleteObject"], Resource = ["${local.app}/compile/*"] },
      { Action = ["s3:GetObject", "s3:PutObject"], Resource = ["${local.native}/*"] },
      { Action = ["s3:ListBucket"], Resource = [local.native] },
    ]
  }
}

# The app bucket holds what teams publish and the apps' KV data; the native bucket, what only the compile function
# writes and every host runs.
resource "aws_s3_bucket" "this" {
  for_each      = toset(["app", "native"])
  bucket        = "${var.name}-${local.account}-${var.region}-${each.key}"
  force_destroy = var.force_destroy
}

resource "aws_lambda_permission" "compile" {
  statement_id   = "markers"
  action         = "lambda:InvokeFunction"
  function_name  = aws_lambda_function.compile.function_name
  principal      = "s3.amazonaws.com"
  source_arn     = local.app
  source_account = local.account
}

resource "aws_s3_bucket_notification" "markers" {
  bucket = aws_s3_bucket.this["app"].id
  lambda_function {
    lambda_function_arn = aws_lambda_function.compile.arn
    events              = ["s3:ObjectCreated:*"]
    filter_prefix       = "compile/"
  }
  depends_on = [aws_lambda_permission.compile]
}

resource "aws_cloudwatch_log_group" "fn" {
  for_each          = local.grants
  name              = "/aws/lambda/${var.name}-${each.key}"
  retention_in_days = var.logs.days
}

resource "aws_iam_role" "fn" {
  for_each = local.grants
  name     = "${var.name}-${each.key}"
  assume_role_policy = jsonencode({
    Version   = "2012-10-17"
    Statement = [{ Effect = "Allow", Action = "sts:AssumeRole", Principal = { Service = "lambda.amazonaws.com" } }]
  })
}

resource "aws_iam_role_policy" "fn" {
  for_each = local.grants
  role     = aws_iam_role.fn[each.key].id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [for s in concat(each.value, [{
      Action   = ["logs:CreateLogStream", "logs:PutLogEvents"]
      Resource = ["${aws_cloudwatch_log_group.fn[each.key].arn}:*"]
    }]) : merge(s, { Effect = "Allow" })]
  })
}

# One zip is both functions: its `bootstrap` runs the subcommand that the handler names.
resource "aws_lambda_function" "compile" {
  function_name                  = "${var.name}-compile"
  handler                        = "compile-worker"
  role                           = aws_iam_role.fn["compile"].arn
  runtime                        = "provided.al2023"
  architectures                  = ["arm64"]
  layers                         = [local.adapter]
  filename                       = local.zip
  source_code_hash               = filebase64sha256(local.zip)
  memory_size                    = 3008
  timeout                        = 120
  reserved_concurrent_executions = var.concurrency.compile
  environment {
    # A 5xx fails the invocation, so Lambda retries the event.
    variables = merge(local.env, { AWS_LWA_ERROR_STATUS_CODES = "500-599" })
  }
  depends_on = [aws_iam_role_policy.fn]
}

resource "aws_lambda_function" "serve" {
  function_name                  = "${var.name}-serve"
  handler                        = "serve"
  role                           = aws_iam_role.fn["serve"].arn
  runtime                        = "provided.al2023"
  architectures                  = ["arm64"]
  layers                         = [local.adapter]
  filename                       = local.zip
  source_code_hash               = filebase64sha256(local.zip)
  memory_size                    = var.serve.memory
  timeout                        = 30
  reserved_concurrent_executions = var.concurrency.serve
  ephemeral_storage { size = var.serve.storage }
  environment { variables = merge(local.env, { AWS_LWA_PORT = "3000" }) }
  # After the compile function, so a new build's hosts ask for native code from a compile function of the same build.
  depends_on = [aws_iam_role_policy.fn, aws_lambda_function.compile]
}

# Public, as CloudFront's origin: one that skips CloudFront can still reach only what CloudFront would route it to.
# The provider adds both of the permissions that a public Function URL needs.
resource "aws_lambda_function_url" "serve" {
  function_name      = aws_lambda_function.serve.function_name
  authorization_type = "NONE"
}

resource "aws_acm_certificate" "this" {
  provider          = aws.us_east_1
  domain_name       = "*.${var.domain}"
  validation_method = "DNS"
  lifecycle { create_before_destroy = true }
}

resource "aws_route53_record" "validation" {
  zone_id         = data.aws_route53_zone.this.zone_id
  name            = one(aws_acm_certificate.this.domain_validation_options).resource_record_name
  type            = one(aws_acm_certificate.this.domain_validation_options).resource_record_type
  records         = [one(aws_acm_certificate.this.domain_validation_options).resource_record_value]
  ttl             = 300
  allow_overwrite = true
}

resource "aws_acm_certificate_validation" "this" {
  provider                = aws.us_east_1
  certificate_arn         = aws_acm_certificate.this.arn
  validation_record_fqdns = [aws_route53_record.validation.fqdn]
}

# The Function URL needs its own `Host`, so the viewer's goes on in `X-Forwarded-Host`, replacing any the viewer sent.
resource "aws_cloudfront_function" "host" {
  name    = "${var.name}-host"
  runtime = "cloudfront-js-2.0"
  code    = <<-EOT
    function handler(event) {
      event.request.headers["x-forwarded-host"] = { value: event.request.headers.host.value };
      return event.request;
    }
  EOT
}

data "aws_cloudfront_cache_policy" "disabled" {
  name = "Managed-CachingDisabled"
}

data "aws_cloudfront_origin_request_policy" "all_but_host" {
  name = "Managed-AllViewerExceptHostHeader"
}

resource "aws_cloudfront_distribution" "this" {
  enabled         = true
  aliases         = ["*.${var.domain}"]
  price_class     = "PriceClass_100"
  http_version    = "http2and3"
  is_ipv6_enabled = true
  origin {
    origin_id   = "serve"
    domain_name = split("/", aws_lambda_function_url.serve.function_url)[2]
    custom_origin_config {
      http_port              = 80
      https_port             = 443
      origin_protocol_policy = "https-only"
      origin_ssl_protocols   = ["TLSv1.2"]
    }
  }
  default_cache_behavior {
    target_origin_id         = "serve"
    viewer_protocol_policy   = "redirect-to-https"
    allowed_methods          = ["GET", "HEAD", "OPTIONS", "PUT", "POST", "PATCH", "DELETE"]
    cached_methods           = ["GET", "HEAD"]
    cache_policy_id          = data.aws_cloudfront_cache_policy.disabled.id
    origin_request_policy_id = data.aws_cloudfront_origin_request_policy.all_but_host.id
    function_association {
      event_type   = "viewer-request"
      function_arn = aws_cloudfront_function.host.arn
    }
  }
  restrictions {
    geo_restriction { restriction_type = "none" }
  }
  viewer_certificate {
    acm_certificate_arn      = aws_acm_certificate_validation.this.certificate_arn
    ssl_support_method       = "sni-only"
    minimum_protocol_version = "TLSv1.2_2021"
  }
}

resource "aws_route53_record" "apps" {
  for_each = toset(["A", "AAAA"])
  zone_id  = data.aws_route53_zone.this.zone_id
  name     = "*.${var.domain}"
  type     = each.key
  alias {
    name                   = aws_cloudfront_distribution.this.domain_name
    zone_id                = aws_cloudfront_distribution.this.hosted_zone_id
    evaluate_target_health = false
  }
}

# A team owns the apps named `<team>-…`: it may read and write them, and the markers that ask for their native code.
# The `team` tag of its role says which; the roles allow no session tags, which could otherwise claim another team.
# Its list is for 404s: S3 answers a missing key with a 403 to a caller that may not list. A GET or HEAD has no
# `s3:prefix`, hence `IfExists`; a list that names a prefix must name one of the team's. (Both checked in M5's session.)
resource "aws_iam_policy" "team" {
  name = "${var.name}-team"
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [for s in [
      { Action = ["s3:GetObject", "s3:PutObject", "s3:DeleteObject"], Resource = ["${local.app}/apps/${local.team}-*"] },
      { Action = ["s3:GetObject", "s3:PutObject"], Resource = ["${local.app}/compile/${local.team}-*"] },
      {
        Action    = ["s3:ListBucket"]
        Resource  = [local.app]
        Condition = { StringLikeIfExists = { "s3:prefix" = ["apps/${local.team}-*", "compile/${local.team}-*"] } }
      },
    ] : merge(s, { Effect = "Allow" })]
  })
}

resource "aws_iam_role" "team" {
  for_each = var.teams
  name     = "${var.name}-team-${each.key}"
  tags     = { team = each.key }
  assume_role_policy = jsonencode({
    Version   = "2012-10-17"
    Statement = [{ Effect = "Allow", Action = "sts:AssumeRole", Principal = { AWS = "arn:aws:iam::${local.account}:root" } }]
  })
}

resource "aws_iam_role_policy_attachment" "team" {
  for_each   = aws_iam_role.team
  role       = each.value.name
  policy_arn = aws_iam_policy.team.arn
}

# The whole account's spend, as the function's URL is public.
resource "aws_budgets_budget" "this" {
  name         = var.name
  budget_type  = "COST"
  limit_amount = var.budget.usd
  limit_unit   = "USD"
  time_unit    = "MONTHLY"
  notification {
    comparison_operator        = "GREATER_THAN"
    threshold                  = 80
    threshold_type             = "PERCENTAGE"
    notification_type          = "ACTUAL"
    subscriber_email_addresses = var.budget.emails
  }
}
