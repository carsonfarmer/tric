# tric on AWS, for a fresh account whose Route 53 already hosts the domain:
# - the router, behind CloudFront at *.<domain>, as two functions from one package: `route`, the function URL, takes
#   only CloudFront's requests; `events` takes only events: cron from Scheduler, and delivery events as `outbox`;
# - serve, with a Lambda tenant per app and no URL;
# - one bucket, versioned.
# Nothing runs while idle. `docker compose run --rm package` builds the package; see infra/aws/README.md.
terraform {
  required_version = ">= 1.8"
  required_providers {
    aws    = { source = "hashicorp/aws", version = ">= 6.28" }
    random = { source = "hashicorp/random", version = ">= 3.6" }
  }
}

variable "domain" {
  description = "Apps are at <app>.<domain>; Route 53 must already host the domain"
  type        = string
}

variable "region" {
  type    = string
  default = "us-west-2"
}

variable "name" {
  description = "The prefix of every resource's name"
  type        = string
  default     = "tric"
}

variable "package" {
  description = "The Lambda package: tric, for arm64, and bootstrap"
  type        = string
  default     = "../../dist/tric.zip"
}

provider "aws" {
  region = var.region
  default_tags { tags = { app = var.name } }
}

# CloudFront takes its certificates from us-east-1 only.
provider "aws" {
  alias  = "us_east_1"
  region = "us-east-1"
  default_tags { tags = { app = var.name } }
}

data "aws_caller_identity" "current" {}

data "aws_route53_zone" "domain" {
  name         = var.domain
  private_zone = false
}

locals {
  # The functions' ARNs, spelt out so that the functions can name each other.
  function = "arn:aws:lambda:${var.region}:${data.aws_caller_identity.current.account_id}:function:${var.name}"
  serve    = "${local.function}-serve"
  events   = "${local.function}-events"
  outbox   = "${local.function}-events:outbox"
  bucket   = aws_s3_bucket.store.arn
  # The Lambda Web Adapter, which turns invocations into HTTP requests to tric.
  adapter = "arn:aws:lambda:${var.region}:753240598075:layer:LambdaAdapterLayerArm64:30"
}

# The bucket: apps/ and native/, and Lambda's on-failure records under aws/lambda/async/, the dead letters.
resource "aws_s3_bucket" "store" {
  bucket_prefix = "${var.name}-"
  force_destroy = true
}

resource "aws_s3_bucket_public_access_block" "store" {
  bucket                  = aws_s3_bucket.store.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_s3_bucket_versioning" "store" {
  bucket = aws_s3_bucket.store.id
  versioning_configuration { status = "Enabled" }
}

resource "aws_s3_bucket_lifecycle_configuration" "store" {
  bucket     = aws_s3_bucket.store.id
  depends_on = [aws_s3_bucket_versioning.store]
  # What a commit or a deploy replaced is kept a day, for the heads that still name it: there is no GC code.
  rule {
    id     = "noncurrent"
    status = "Enabled"
    filter {}
    noncurrent_version_expiration { noncurrent_days = 1 }
    expiration { expired_object_delete_marker = true }
    abort_incomplete_multipart_upload { days_after_initiation = 1 }
  }
  rule {
    id     = "dead"
    status = "Enabled"
    filter { prefix = "aws/lambda/async/" }
    expiration { days = 14 }
  }
}

# The roles. Policies are written out, so that the module's tests read them as they are.
locals {
  lambda = jsonencode({
    Version   = "2012-10-17"
    Statement = [{ Effect = "Allow", Principal = { Service = "lambda.amazonaws.com" }, Action = "sts:AssumeRole" }]
  })
  logs = ["logs:CreateLogStream", "logs:PutLogEvents"]
}

# The router's: the trusted core, which mints the apps' credentials.
resource "aws_iam_role" "route" {
  name               = "${var.name}-route"
  assume_role_policy = local.lambda
}

resource "aws_iam_role_policy" "route" {
  role = aws_iam_role.route.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect   = "Allow"
        Action   = local.logs
        Resource = [for f in ["route", "events"] : "${aws_cloudwatch_log_group.function[f].arn}:*"]
      },
      { Effect = "Allow", Action = "sts:AssumeRole", Resource = aws_iam_role.app.arn },
      { Effect = "Allow", Action = "s3:GetObject", Resource = "${local.bucket}/apps/*/current" },
      { Effect = "Allow", Action = "lambda:InvokeFunction", Resource = local.serve },
      # The outbox alias's on-failure records, which Lambda writes as the function under `aws/lambda/async/`. Lambda
      # takes the destination only if the role may write the whole bucket.
      { Effect = "Allow", Action = "s3:PutObject", Resource = "${local.bucket}/*" },
      { Effect = "Allow", Action = "s3:ListBucket", Resource = local.bucket },
    ]
  })
}

# The apps': the router's sessions narrow it to one app.
resource "aws_iam_role" "app" {
  name = "${var.name}-app"
  assume_role_policy = jsonencode({
    Version   = "2012-10-17"
    Statement = [{ Effect = "Allow", Principal = { AWS = aws_iam_role.route.arn }, Action = "sts:AssumeRole" }]
  })
}

resource "aws_iam_role_policy" "app" {
  role = aws_iam_role.app.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect   = "Allow"
        Action   = ["s3:GetObject", "s3:GetObjectVersion"]
        Resource = ["${local.bucket}/apps/*", "${local.bucket}/native/*"]
      },
      { # never a release or a component, which only deploy writes
        Effect   = "Allow"
        Action   = ["s3:PutObject", "s3:DeleteObject"]
        Resource = ["${local.bucket}/apps/*/names/*", "${local.bucket}/apps/*/values/*", "${local.bucket}/native/*"]
      },
    ]
  })
}

# serve's: its logs and the outbox, and no storage at all.
resource "aws_iam_role" "serve" {
  name               = "${var.name}-serve"
  assume_role_policy = local.lambda
}

resource "aws_iam_role_policy" "serve" {
  role = aws_iam_role.serve.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      { Effect = "Allow", Action = local.logs, Resource = "${aws_cloudwatch_log_group.function["serve"].arn}:*" },
      { Effect = "Allow", Action = "lambda:InvokeFunction", Resource = local.outbox },
    ]
  })
}

# Scheduler's, for the apps' cron: the events function, unqualified, which takes cron and not delivery events.
resource "aws_iam_role" "scheduler" {
  name = "${var.name}-scheduler"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "scheduler.amazonaws.com" }
      Action    = "sts:AssumeRole"
      Condition = {
        StringEquals = {
          "aws:SourceAccount" = data.aws_caller_identity.current.account_id
          "aws:SourceArn"     = aws_scheduler_schedule_group.cron.arn
        }
      }
    }]
  })
}

resource "aws_iam_role_policy" "scheduler" {
  role = aws_iam_role.scheduler.id
  policy = jsonencode({
    Version   = "2012-10-17"
    Statement = [{ Effect = "Allow", Action = "lambda:InvokeFunction", Resource = local.events }]
  })
}

resource "aws_scheduler_schedule_group" "cron" {
  name = var.name
}

# The functions, from one package: bootstrap runs `tric <handler>`, behind the adapter, on port 3000.
resource "random_password" "origin" {
  length  = 40
  special = false
}

locals {
  router = {
    TRIC_DOMAIN = var.domain
    TRIC_BUCKET = aws_s3_bucket.store.id
    TRIC_SERVE  = local.serve
    TRIC_ROLE   = aws_iam_role.app.arn
  }
  functions = {
    # Clients' requests, which only CloudFront's secret lets in.
    route = {
      role    = aws_iam_role.route.arn
      handler = "route"
      memory  = 256
      timeout = 360
      env = merge(local.router, {
        TRIC_ORIGIN         = random_password.origin.result
        AWS_LWA_INVOKE_MODE = "response_stream"
      })
    }
    # Events only. A 5xx fails the invocation, so that Lambda retries a delivery.
    events = {
      role    = aws_iam_role.route.arn
      handler = "route"
      memory  = 256
      timeout = 360
      env     = merge(local.router, { AWS_LWA_ERROR_STATUS_CODES = "500-599" })
    }
    serve = {
      role    = aws_iam_role.serve.arn
      handler = "serve"
      memory  = 1024
      timeout = 330
      env = {
        TRIC_DOMAIN         = var.domain
        TRIC_BUCKET         = aws_s3_bucket.store.id
        TRIC_OUTBOX         = local.outbox
        AWS_LWA_INVOKE_MODE = "response_stream"
      }
    }
  }
}

resource "aws_cloudwatch_log_group" "function" {
  for_each          = local.functions
  name              = "/aws/lambda/${var.name}-${each.key}"
  retention_in_days = 3
}

resource "aws_lambda_function" "function" {
  for_each         = local.functions
  function_name    = "${var.name}-${each.key}"
  role             = each.value.role
  runtime          = "provided.al2023"
  architectures    = ["arm64"]
  handler          = each.value.handler
  filename         = var.package
  source_code_hash = filebase64sha256(var.package)
  layers           = [local.adapter]
  memory_size      = each.value.memory
  timeout          = each.value.timeout
  environment { variables = merge(each.value.env, { AWS_LWA_PORT = "3000" }) }
  # An app per tenant: a Lambda execution environment never runs two apps.
  dynamic "tenancy_config" {
    for_each = each.key == "serve" ? [1] : []
    content { tenant_isolation_mode = "PER_TENANT" }
  }
  depends_on = [aws_cloudwatch_log_group.function, aws_iam_role_policy.route, aws_iam_role_policy.serve]
}

# serve invokes this alias with delivery events: retried twice, kept up to 6 hours, then written to the bucket.
resource "aws_lambda_alias" "outbox" {
  name             = "outbox"
  function_name    = aws_lambda_function.function["events"].function_name
  function_version = "$LATEST"
}

resource "aws_lambda_function_event_invoke_config" "outbox" {
  function_name                = aws_lambda_function.function["events"].function_name
  qualifier                    = aws_lambda_alias.outbox.name
  maximum_retry_attempts       = 2
  maximum_event_age_in_seconds = 21600
  destination_config {
    on_failure { destination = local.bucket }
  }
}

# Public, as CloudFront's origin: the provider grants InvokeFunctionUrl and, through the URL only, InvokeFunction.
resource "aws_lambda_function_url" "route" {
  function_name      = aws_lambda_function.function["route"].function_name
  authorization_type = "NONE"
  invoke_mode        = "RESPONSE_STREAM"
}

# CloudFront, at *.<domain>.
resource "aws_acm_certificate" "apps" {
  provider          = aws.us_east_1
  domain_name       = "*.${var.domain}"
  validation_method = "DNS"
  lifecycle { create_before_destroy = true }
}

locals {
  validation = one(aws_acm_certificate.apps.domain_validation_options)
}

resource "aws_route53_record" "validation" {
  zone_id         = data.aws_route53_zone.domain.zone_id
  name            = local.validation.resource_record_name
  type            = local.validation.resource_record_type
  records         = [local.validation.resource_record_value]
  ttl             = 300
  allow_overwrite = true
}

resource "aws_acm_certificate_validation" "apps" {
  provider                = aws.us_east_1
  certificate_arn         = aws_acm_certificate.apps.arn
  validation_record_fqdns = [aws_route53_record.validation.fqdn]
}

# The origin gets the function URL's Host, so the app's goes as X-Forwarded-Host, replacing any the viewer sent.
resource "aws_cloudfront_function" "host" {
  name    = "${var.name}-host"
  runtime = "cloudfront-js-2.0"
  publish = true
  code    = <<-EOT
    function handler(event) {
      event.request.headers["x-forwarded-host"] = { value: event.request.headers.host.value };
      return event.request;
    }
  EOT
}

resource "aws_cloudfront_distribution" "apps" {
  enabled         = true
  is_ipv6_enabled = true
  http_version    = "http2and3"
  price_class     = "PriceClass_100"
  aliases         = ["*.${var.domain}"]
  origin {
    origin_id   = "route"
    domain_name = split("/", aws_lambda_function_url.route.function_url)[2]
    custom_origin_config {
      http_port              = 80
      https_port             = 443
      origin_protocol_policy = "https-only"
      origin_ssl_protocols   = ["TLSv1.2"]
      origin_read_timeout    = 60
    }
    # The secret without which the router refuses a request; CloudFront replaces any the viewer sent.
    custom_header {
      name  = "X-Tric-Origin"
      value = random_password.origin.result
    }
  }
  default_cache_behavior {
    target_origin_id       = "route"
    viewer_protocol_policy = "redirect-to-https"
    allowed_methods        = ["GET", "HEAD", "OPTIONS", "PUT", "POST", "PATCH", "DELETE"]
    cached_methods         = ["GET", "HEAD"]
    # CachingDisabled; and AllViewerExceptHostHeader, which adds CloudFront-Viewer-Address, the viewer's address.
    cache_policy_id          = "4135ea2d-6df8-44a3-9df3-4b5a84be39ad"
    origin_request_policy_id = "b689b0a8-53d0-40ab-baf2-68738e2966ac"
    function_association {
      event_type   = "viewer-request"
      function_arn = aws_cloudfront_function.host.arn
    }
  }
  restrictions {
    geo_restriction { restriction_type = "none" }
  }
  viewer_certificate {
    acm_certificate_arn      = aws_acm_certificate_validation.apps.certificate_arn
    ssl_support_method       = "sni-only"
    minimum_protocol_version = "TLSv1.2_2021"
  }
}

resource "aws_route53_record" "apps" {
  for_each        = toset(["A", "AAAA"])
  zone_id         = data.aws_route53_zone.domain.zone_id
  name            = "*.${var.domain}"
  type            = each.key
  allow_overwrite = true
  alias {
    name                   = aws_cloudfront_distribution.apps.domain_name
    zone_id                = aws_cloudfront_distribution.apps.hosted_zone_id
    evaluate_target_health = false
  }
}

# `tric deploy`'s environment.
output "TRIC_BUCKET" {
  value = aws_s3_bucket.store.id
}

output "TRIC_SCHEDULES" {
  value = aws_scheduler_schedule_group.cron.name
}

output "TRIC_EVENTS" {
  value = aws_lambda_function.function["events"].arn
}

output "TRIC_SCHEDULER_ROLE" {
  value = aws_iam_role.scheduler.arn
}

output "AWS_REGION" {
  value = var.region
}
