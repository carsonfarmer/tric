# tric on AWS, for an account whose Route 53 already hosts the domain: see README.md. Nothing runs while idle.
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
  description = "The prefix of every name the install makes, so that two installs can share an account"
  type        = string
  default     = "tric"
  validation {
    # The bucket's name is the name, `-`, and a suffix, and S3 takes lowercase letters, digits and hyphens only.
    condition     = can(regex("^[a-z][a-z0-9-]{0,31}$", var.name))
    error_message = "name is a lowercase letter, then at most 31 lowercase letters, digits and hyphens"
  }
}

variable "package" {
  description = "The Lambda package: tric, for arm64, and bootstrap"
  type        = string
  default     = "../../dist/tric.zip"
}

variable "route_concurrency" {
  description = "The most client requests run at once, reserved from the account's concurrency; -1 for no cap"
  type        = number
  default     = 200
}

variable "log" {
  description = "What each function logs, as RUST_LOG; `warn,tric=debug` adds every read and write of a head"
  type        = string
  default     = "warn,tric=info"
}

variable "ws_rate" {
  description = "Sockets' events a second, opening, messages and closing, across every app, as API Gateway throttles"
  type        = number
  default     = 100
}

variable "ws_burst" {
  description = "Sockets' events at once, above `ws_rate`, as API Gateway throttles"
  type        = number
  default     = 200
}

variable "noncurrent_days" {
  description = "How many days a replaced or deleted version is kept: the history the sweep keeps, and so the rollback"
  type        = number
  default     = 1
  validation {
    condition     = var.noncurrent_days >= 1 && var.noncurrent_days == floor(var.noncurrent_days)
    error_message = "noncurrent_days is a whole number of days, at least 1."
  }
}

variable "sweep_every" {
  description = "How often each app is swept, as `tric deploy` schedules it: hours that divide a day, as `6h`, or days"
  type        = string
  default     = "1d"
  validation {
    condition     = can(regex("^(1|2|3|4|6|8|12|24)h$|^([1-9]|[12][0-9]|3[01])d$", var.sweep_every))
    error_message = "sweep_every is hours that divide a day (1h, 2h, 3h, 4h, 6h, 8h, 12h or 24h), or 1d to 31d."
  }
}

variable "tenancy" {
  description = <<-EOT
    Whether serve runs each app in tenants of its own (PER_TENANT), so an execution environment never runs two apps.
    Off, serve's execution environments are shared across apps: for test accounts that can't make PER_TENANT functions
    only, never for apps that must be kept apart.
  EOT
  type        = bool
  default     = true
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
  outbox   = "${local.function}-events:outbox"
  retry    = "${local.function}-events:retry"
  bucket   = aws_s3_bucket.store.arn
  # API Gateway's, for sockets: its stage `ws`, which CloudFront sends upgrades to, and `@connections`'.
  api = "${aws_apigatewayv2_api.ws.id}.execute-api.${var.region}.amazonaws.com"
  ws  = "${aws_apigatewayv2_api.ws.execution_arn}/ws"
  # The Lambda Web Adapter, which turns invocations into HTTP requests to tric.
  adapter = "arn:aws:lambda:${var.region}:753240598075:layer:LambdaAdapterLayerArm64:30"
  # The retries' schedule group, and its schedules, whose names the router makes up.
  retries   = aws_scheduler_schedule_group.scheduler["retry"].name
  schedules = "arn:aws:scheduler:${var.region}:${data.aws_caller_identity.current.account_id}:schedule/${local.retries}"
}

# The bucket: apps/ and native/; and ws/, sockets' records, and outbox/, the deliveries that wait to be tried again,
# which only the router reads and writes.
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
  # A head's noncurrent versions are the sweep's history: it keeps whatever any of them names, so a name, even one
  # deleted, could be rolled back to any of them for as long as this rule keeps them. The rule also expires what the
  # sweep deleted and what a deploy replaced, which a versioned bucket keeps as noncurrent versions. S3 counts whole
  # days to the next midnight UTC, so a version is kept from `noncurrent_days` to a day more: 1 is 24 to 48 hours. One
  # rule serves both: a filter is a literal prefix, and each app's heads are under its own, so heads kept longer than
  # the rest need a key layout that puts every app's heads under one prefix.
  rule {
    id     = "noncurrent"
    status = "Enabled"
    filter {}
    noncurrent_version_expiration { noncurrent_days = var.noncurrent_days }
    expiration { expired_object_delete_marker = true }
    abort_incomplete_multipart_upload { days_after_initiation = 1 }
  }
  # A delivery is tried again for 24 hours, and the router deletes it then, so what is left in 2 days is a safety net.
  rule {
    id     = "outbox"
    status = "Enabled"
    filter { prefix = "outbox/" }
    expiration { days = 2 }
  }
  # A socket lasts 2 hours at most, so a record that its closing never deleted is gone a day on.
  rule {
    id     = "ws"
    status = "Enabled"
    filter { prefix = "ws/" }
    expiration { days = 1 }
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
      # What the router writes: sockets' records, and the deliveries that wait. It lists a channel's sockets, no more.
      {
        Effect   = "Allow"
        Action   = ["s3:GetObject", "s3:PutObject", "s3:DeleteObject"]
        Resource = ["${local.bucket}/ws/*", "${local.bucket}/outbox/*"]
      },
      {
        Effect    = "Allow"
        Action    = "s3:ListBucket"
        Resource  = local.bucket
        Condition = { StringLike = { "s3:prefix" = "ws/channels/*" } }
      },
      { Effect = "Allow", Action = "lambda:InvokeFunction", Resource = local.serve },
      # Sends to, and closes, this API's sockets, and no other's.
      {
        Effect   = "Allow"
        Action   = "execute-api:ManageConnections"
        Resource = ["${local.ws}/POST/@connections/*", "${local.ws}/DELETE/@connections/*"]
      },
      # Schedules a delivery's next try in the retry group alone, as the retry role alone.
      {
        Effect   = "Allow"
        Action   = "scheduler:CreateSchedule"
        Resource = "${local.schedules}/*"
      },
      {
        Effect    = "Allow"
        Action    = "iam:PassRole"
        Resource  = aws_iam_role.scheduler["retry"].arn
        Condition = { StringEquals = { "iam:PassedToService" = "scheduler.amazonaws.com" } }
      },
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
      # For the sweep, and no more: it lists its values, and the versions of its names, which are their history. A list
      # shows the keys under its prefix; no version is ever the app's to delete.
      {
        Effect    = "Allow"
        Action    = ["s3:ListBucket"]
        Resource  = [local.bucket]
        Condition = { StringLike = { "s3:prefix" = ["apps/*/values/*"] } }
      },
      {
        Effect    = "Allow"
        Action    = ["s3:ListBucketVersions"]
        Resource  = [local.bucket]
        Condition = { StringLike = { "s3:prefix" = ["apps/*/names/*"] } }
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

# Scheduler's, a group and a role for each alias of the events function that it invokes: `cron`, for the apps' cron and
# sweeps, and `retry`, for the deliveries' retries. A role invokes its own alias and no other, and only its own group's
# schedules may assume it. The retries' group is apart, so that the router, which makes their schedules, can make none
# that run as `cron`.
resource "aws_scheduler_schedule_group" "scheduler" {
  for_each = toset(["cron", "retry"])
  name     = "${var.name}-${each.key}"
}

resource "aws_iam_role" "scheduler" {
  for_each = aws_scheduler_schedule_group.scheduler
  name     = "${var.name}-${each.key}"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "scheduler.amazonaws.com" }
      Action    = "sts:AssumeRole"
      Condition = {
        StringEquals = {
          "aws:SourceAccount" = data.aws_caller_identity.current.account_id
          "aws:SourceArn"     = each.value.arn
        }
      }
    }]
  })
}

resource "aws_iam_role_policy" "scheduler" {
  for_each = aws_scheduler_schedule_group.scheduler
  role     = aws_iam_role.scheduler[each.key].id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect   = "Allow"
      Action   = "lambda:InvokeFunction"
      Resource = "${local.function}-events:${each.key}"
    }]
  })
}

# The functions, from one package: bootstrap runs `tric <handler>`, behind the adapter, on port 3000.
resource "random_password" "origin" {
  length  = 40
  special = false
}

locals {
  # The secret is the router's on both: CloudFront sends it with every request, and with every socket's opening.
  router = {
    TRIC_DOMAIN = var.domain
    TRIC_BUCKET = aws_s3_bucket.store.id
    TRIC_SERVE  = local.serve
    TRIC_ROLE   = aws_iam_role.app.arn
    TRIC_ORIGIN = random_password.origin.result
    # Whether serve has tenancy, so that the router names the app as Lambda's tenant when it invokes serve, or not.
    TRIC_TENANCY = tostring(var.tenancy)
    # Where a delivery that fails waits for its next try: the group the router schedules in, and what it schedules.
    TRIC_SCHEDULER_GROUP  = local.retries
    TRIC_SCHEDULER_TARGET = local.retry
    TRIC_SCHEDULER_ROLE   = aws_iam_role.scheduler["retry"].arn
  }
  functions = {
    # Clients' requests, which only CloudFront's secret lets in.
    route = {
      role    = aws_iam_role.route.arn
      handler = "route"
      memory  = 256
      timeout = 360
      env     = merge(local.router, { AWS_LWA_INVOKE_MODE = "response_stream" })
    }
    # Events only, buffered, as API Gateway reads a socket's answer whole. A 5xx fails the invocation, so that Lambda
    # tries it again if the router itself failed. `TRIC_WS` is the stage's URL, where `@connections` is.
    events = {
      role    = aws_iam_role.route.arn
      handler = "route"
      memory  = 256
      timeout = 360
      env     = merge(local.router, { TRIC_WS = "https://${local.api}/ws", AWS_LWA_ERROR_STATUS_CODES = "500-599" })
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
  # A flood of client requests holds at most this many routers, and as many serves, so the rest of the account's
  # concurrency stays for cron and the outbox. Reserving keeps nothing warm: it costs nothing while idle.
  reserved_concurrent_executions = each.key == "route" ? var.route_concurrency : -1
  environment { variables = merge(each.value.env, { AWS_LWA_PORT = "3000", RUST_LOG = var.log }) }
  # An app per tenant: a Lambda execution environment never runs two apps. Without tenancy, for test accounts only,
  # serve's execution environments are shared across apps.
  dynamic "tenancy_config" {
    for_each = each.key == "serve" && var.tenancy ? [1] : []
    content { tenant_isolation_mode = "PER_TENANT" }
  }
  depends_on = [aws_cloudwatch_log_group.function, aws_iam_role_policy.route, aws_iam_role_policy.serve]
}

# The events function's aliases, one for each source, as the router takes no event that names no alias: serve invokes
# `outbox` with delivery events, Scheduler `cron` with the apps' cron and `retry` with a delivery's retries, as roles of
# their own, and API Gateway `ws` with sockets' events.
resource "aws_lambda_alias" "events" {
  for_each         = toset(["outbox", "cron", "retry", "ws"])
  name             = each.key
  function_name    = aws_lambda_function.function["events"].function_name
  function_version = "$LATEST"
}

# A delivery that the app fails waits for Scheduler, and these are for when the router itself fails: Lambda tries the
# event twice more, for up to 6 hours, and then drops it. There is no destination, which would need the router to write
# the whole bucket.
resource "aws_lambda_function_event_invoke_config" "events" {
  for_each                     = toset(["outbox", "retry"])
  function_name                = aws_lambda_function.function["events"].function_name
  qualifier                    = aws_lambda_alias.events[each.key].name
  maximum_retry_attempts       = 2
  maximum_event_age_in_seconds = 21600
}

# API Gateway alone may invoke `ws`.
resource "aws_lambda_permission" "ws" {
  statement_id  = "ws"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.function["events"].function_name
  qualifier     = aws_lambda_alias.events["ws"].name
  principal     = "apigateway.amazonaws.com"
  source_arn    = "${local.ws}/*"
}

# The sockets, which API Gateway holds: every event goes to the router, whose answers carry no messages, as there is no
# route response; every message to a client goes through `@connections`. Its own URL refuses an opening without the
# origin secret, as the function URL does a request.
resource "aws_apigatewayv2_api" "ws" {
  name                       = "${var.name}-ws"
  protocol_type              = "WEBSOCKET"
  route_selection_expression = "$request.body.action"
}

resource "aws_apigatewayv2_integration" "ws" {
  api_id             = aws_apigatewayv2_api.ws.id
  integration_type   = "AWS_PROXY"
  integration_method = "POST"
  integration_uri    = aws_lambda_alias.events["ws"].invoke_arn
}

resource "aws_apigatewayv2_route" "ws" {
  for_each  = toset(["$connect", "$default", "$disconnect"])
  api_id    = aws_apigatewayv2_api.ws.id
  route_key = each.key
  target    = "integrations/${aws_apigatewayv2_integration.ws.id}"
}

# Messages cost $1 a million, so the throttle bounds what a flood costs.
resource "aws_apigatewayv2_stage" "ws" {
  api_id      = aws_apigatewayv2_api.ws.id
  name        = "ws"
  auto_deploy = true
  default_route_settings {
    throttling_rate_limit  = var.ws_rate
    throttling_burst_limit = var.ws_burst
  }
  depends_on = [aws_apigatewayv2_route.ws]
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

# The origin gets the function URL's Host, so the app's goes as X-Forwarded-Host, replacing any the viewer sent. A
# function can't see `Upgrade`, so a request with `Sec-WebSocket-Key` is taken as a socket's opening, and goes to API
# Gateway's stage, with its path as X-Forwarded-Path; its query goes as it is.
resource "aws_cloudfront_function" "host" {
  name    = "${var.name}-host"
  runtime = "cloudfront-js-2.0"
  publish = true
  code    = <<-EOT
    import cf from 'cloudfront';
    function handler(event) {
      var request = event.request, headers = request.headers;
      headers["x-forwarded-host"] = { value: headers.host.value };
      if (headers["sec-websocket-key"]) {
        headers["x-forwarded-path"] = { value: request.uri };
        request.uri = "/ws";
        cf.updateRequestOrigin({
          domainName: "${local.api}",
          customOriginConfig: { port: 443, protocol: "https", sslProtocols: ["TLSv1.2"] },
        });
      }
      return request;
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

output "TRIC_SCHEDULER_GROUP" {
  value = aws_scheduler_schedule_group.scheduler["cron"].name
}

output "TRIC_SCHEDULER_TARGET" {
  value = aws_lambda_alias.events["cron"].arn
}

output "TRIC_SCHEDULER_ROLE" {
  value = aws_iam_role.scheduler["cron"].arn
}

output "TRIC_SWEEP_EVERY" {
  value = var.sweep_every
}

output "AWS_REGION" {
  value = var.region
}

# The checks' (check.sh): the prefix of every name, and whether serve has tenancy.
output "name" {
  value = var.name
}

output "tenancy" {
  value = var.tenancy
}
