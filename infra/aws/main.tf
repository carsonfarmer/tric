# An install of tric: one bucket, one function, CloudFront in front of it at *.<domain>, a Scheduler group for the apps'
# cron, and a budget alert. The function serves every app, and is invoked as well by its own outbox and by Scheduler.

data "aws_caller_identity" "current" {}

data "aws_route53_zone" "this" {
  name = var.domain
}

locals {
  account = data.aws_caller_identity.current.account_id
  bucket  = aws_s3_bucket.this.arn
  # Built from its parts, as the function's own role names it, and the role must exist before the function.
  function = "arn:aws:lambda:${var.region}:${local.account}:function:${var.name}"
  # The Lambda Web Adapter, published by AWS: an extension that turns invocations into HTTP requests to the function.
  adapter = "arn:aws:lambda:${var.region}:753240598075:layer:LambdaAdapterLayerArm64:30"
  zip     = "${path.module}/../../dist/tric.zip" # as `docker compose run --rm release` builds it
  # The one record that proves the domain is ours, as the certificate is for one name.
  validation = one(aws_acm_certificate.this.domain_validation_options)
}

# Everything tric keeps: apps, their releases and components, native code, the names' state, and `install`.
resource "aws_s3_bucket" "this" {
  bucket        = "${var.name}-${local.account}-${var.region}"
  force_destroy = var.force_destroy
}

# Versioned, as a name's head pins each large value it holds by version, so a snapshot can read a value that a later
# turn replaced. What is overwritten or deleted is kept for 7 days, so the operator can put it back.
resource "aws_s3_bucket_versioning" "this" {
  bucket = aws_s3_bucket.this.id
  versioning_configuration { status = "Enabled" }
}

resource "aws_s3_bucket_lifecycle_configuration" "this" {
  bucket = aws_s3_bucket.this.id
  rule {
    id     = "versions"
    status = "Enabled"
    filter {}
    noncurrent_version_expiration { noncurrent_days = 7 }
    expiration { expired_object_delete_marker = true }
    abort_incomplete_multipart_upload { days_after_initiation = 1 }
  }
  depends_on = [aws_s3_bucket_versioning.this]
}

# The bucket's own policy binds every principal, the operator too: it takes only TLS. S3's defaults already block public
# access, turn off ACLs and encrypt at rest.
resource "aws_s3_bucket_policy" "this" {
  bucket = aws_s3_bucket.this.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Sid       = "TLSOnly"
      Effect    = "Deny"
      Principal = "*"
      Action    = ["s3:*"]
      Resource  = [local.bucket, "${local.bucket}/*"]
      Condition = { Bool = { "aws:SecureTransport" = "false" } }
    }]
  })
}

resource "aws_cloudwatch_log_group" "this" {
  name              = "/aws/lambda/${var.name}"
  retention_in_days = var.logs.days
}

resource "aws_iam_role" "function" {
  name = var.name
  assume_role_policy = jsonencode({
    Version   = "2012-10-17"
    Statement = [{ Effect = "Allow", Action = "sts:AssumeRole", Principal = { Service = "lambda.amazonaws.com" } }]
  })
}

# The bucket, which Lambda's records of failed events go to as well; invoking itself with an outbox; and its logs.
resource "aws_iam_role_policy" "function" {
  role = aws_iam_role.function.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [for s in [
      # GetObjectVersion, for a large value pinned by version.
      {
        Action   = ["s3:GetObject", "s3:GetObjectVersion", "s3:PutObject", "s3:DeleteObject"]
        Resource = ["${local.bucket}/*"]
      },
      { Action = ["s3:ListBucket"], Resource = [local.bucket] }, # so a missing key is a 404, not a 403
      { Action = ["lambda:InvokeFunction"], Resource = [local.function] },
      { Action = ["logs:CreateLogStream", "logs:PutLogEvents"], Resource = ["${aws_cloudwatch_log_group.this.arn}:*"] },
    ] : merge(s, { Effect = "Allow" })]
  })
}

# One function for everything: `bootstrap` runs `tric serve`, as the handler says, behind the adapter.
resource "aws_lambda_function" "this" {
  function_name                  = var.name
  handler                        = "serve"
  role                           = aws_iam_role.function.arn
  runtime                        = "provided.al2023"
  architectures                  = ["arm64"]
  layers                         = [local.adapter]
  filename                       = local.zip
  source_code_hash               = filebase64sha256(local.zip)
  memory_size                    = var.memory
  timeout                        = 900 # an outbox's delivery, which waits for its turn's commit and retries
  reserved_concurrent_executions = var.concurrency
  environment {
    variables = {
      TRIC_STORE  = "s3://${aws_s3_bucket.this.bucket}"
      TRIC_DOMAIN = var.domain
      TRIC_LISTEN = "127.0.0.1:8080" # where the adapter sends requests
      RUST_LOG    = var.logs.filter
      # A TCP check, as an HTTP one would be a request to no app. No AWS_LWA_ERROR_STATUS_CODES: it would turn an app's
      # 5xx into a 502, so Lambda retries an event only when the function crashes or times out.
      AWS_LWA_READINESS_CHECK_PROTOCOL = "tcp"
      AWS_LWA_INVOKE_MODE              = "response_stream" # as the Function URL's
    }
  }
  depends_on = [aws_iam_role_policy.function, aws_cloudwatch_log_group.this]
}

# An outbox or cron event whose invocation crashed or timed out is tried twice more, and then kept in the bucket, at
# `aws/lambda/async/<function>/<yyyy>/<mm>/<dd>/…`, where Lambda puts it.
resource "aws_lambda_function_event_invoke_config" "this" {
  function_name                = aws_lambda_function.this.function_name
  maximum_retry_attempts       = 2
  maximum_event_age_in_seconds = 21600
  destination_config {
    on_failure { destination = local.bucket }
  }
}

# Public, as CloudFront's origin: one that skips CloudFront can still reach only what CloudFront would route it to.
# The provider adds both of the permissions that a public Function URL needs. Streamed, so a response's head reaches
# CloudFront within the 10 s an app has to answer, inside CloudFront's 30 s, and its body may take the 300 s it may.
resource "aws_lambda_function_url" "this" {
  function_name      = aws_lambda_function.this.function_name
  authorization_type = "NONE"
  invoke_mode        = "RESPONSE_STREAM"
}

# The apps' cron: `tric deploy` and `tric release` keep a schedule in this group for each of an app's expressions, which
# invokes the function with the role below. `install` tells them which.
resource "aws_scheduler_schedule_group" "this" {
  name = var.name
}

resource "aws_iam_role" "scheduler" {
  name = "${var.name}-scheduler"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Action    = "sts:AssumeRole"
      Principal = { Service = "scheduler.amazonaws.com" }
      Condition = { StringEquals = { "aws:SourceAccount" = local.account } }
    }]
  })
}

resource "aws_iam_role_policy" "scheduler" {
  role = aws_iam_role.scheduler.id
  policy = jsonencode({
    Version   = "2012-10-17"
    Statement = [{ Effect = "Allow", Action = ["lambda:InvokeFunction"], Resource = [aws_lambda_function.this.arn] }]
  })
}

resource "aws_s3_object" "install" {
  bucket       = aws_s3_bucket.this.id
  key          = "install"
  content_type = "application/json"
  content = jsonencode({
    function = aws_lambda_function.this.arn
    role     = aws_iam_role.scheduler.arn
    group    = aws_scheduler_schedule_group.this.name
  })
}

# What the CLI needs to deploy, release and set the environment of any app, for the operator to attach to whoever
# deploys. Its writes are under `apps/`, so a deployer cannot write native code, which hosts load unchecked: what a
# deployer ships runs in the sandbox.
resource "aws_iam_policy" "deploy" {
  name = "${var.name}-deploy"
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [for s in [
      { Action = ["s3:GetObject", "s3:PutObject"], Resource = ["${local.bucket}/apps/*"] },
      { Action = ["s3:GetObject"], Resource = ["${local.bucket}/install"] },
      { Action = ["s3:ListBucket"], Resource = [local.bucket] },
      { Action = ["scheduler:ListSchedules"], Resource = ["*"] }, # which takes no resource
      {
        Action   = ["scheduler:CreateSchedule", "scheduler:DeleteSchedule"]
        Resource = ["arn:aws:scheduler:${var.region}:${local.account}:schedule/${var.name}/*"]
      },
      {
        Action    = ["iam:PassRole"]
        Resource  = [aws_iam_role.scheduler.arn]
        Condition = { StringEquals = { "iam:PassedToService" = "scheduler.amazonaws.com" } }
      },
    ] : merge(s, { Effect = "Allow" })]
  })
}

resource "aws_acm_certificate" "this" {
  provider          = aws.us_east_1
  domain_name       = "*.${var.domain}"
  validation_method = "DNS"
  lifecycle { create_before_destroy = true }
}

resource "aws_route53_record" "validation" {
  zone_id         = data.aws_route53_zone.this.zone_id
  name            = local.validation.resource_record_name
  type            = local.validation.resource_record_type
  records         = [local.validation.resource_record_value]
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
    origin_id   = "function"
    domain_name = split("/", aws_lambda_function_url.this.function_url)[2]
    custom_origin_config {
      http_port              = 80
      https_port             = 443
      origin_protocol_policy = "https-only"
      origin_ssl_protocols   = ["TLSv1.2"]
    }
  }
  default_cache_behavior {
    target_origin_id         = "function"
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

# The whole account's spend, as the function's URL is public.
resource "aws_budgets_budget" "this" {
  name         = var.name
  budget_type  = "COST"
  limit_amount = var.budget.usd
  limit_unit   = "USD"
  time_unit    = "MONTHLY"
  cost_types {
    include_credit = false # which would hide the spend until they ran out
  }
  notification {
    comparison_operator        = "GREATER_THAN"
    threshold                  = 80
    threshold_type             = "PERCENTAGE"
    notification_type          = "ACTUAL"
    subscriber_email_addresses = var.budget.emails
  }
}
