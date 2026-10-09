# The module's checks, against mock providers: nothing here reaches AWS. `tofu test`, after `tofu init`.
mock_provider "aws" {
  mock_data "aws_caller_identity" {
    defaults = { account_id = "123456789012" }
  }
  mock_resource "aws_lambda_function_url" {
    defaults = { function_url = "https://route.lambda-url.us-west-2.on.aws/" }
  }
  mock_resource "aws_s3_bucket" {
    defaults = { arn = "arn:aws:s3:::tric-x", id = "tric-x" }
  }
  mock_resource "aws_cloudwatch_log_group" {
    defaults = { arn = "arn:aws:logs:us-west-2:123456789012:log-group:x" }
  }
  mock_resource "aws_scheduler_schedule_group" {
    defaults = { arn = "arn:aws:scheduler:us-west-2:123456789012:schedule-group/tric" }
  }
  mock_resource "aws_cloudfront_function" {
    defaults = { arn = "arn:aws:cloudfront::123456789012:function/tric-host" }
  }
  mock_resource "aws_apigatewayv2_api" {
    defaults = { id = "a1b2c3", execution_arn = "arn:aws:execute-api:us-west-2:123456789012:a1b2c3" }
  }
  mock_resource "aws_iam_role" {
    defaults = { arn = "arn:aws:iam::123456789012:role/tric-x" }
  }
  mock_resource "aws_lambda_alias" {
    defaults = { invoke_arn = "arn:aws:apigateway:us-west-2:lambda:path/x/invocations" }
  }
}

# The router's role apart from the rest, so that the app role's trust is seen to name it.
override_resource {
  target = aws_iam_role.route
  values = { arn = "arn:aws:iam::123456789012:role/tric-route" }
}

mock_provider "aws" {
  alias = "us_east_1"
  mock_resource "aws_acm_certificate" {
    defaults = {
      arn = "arn:aws:acm:us-east-1:123456789012:certificate/x"
      domain_validation_options = [{
        domain_name           = "*.tric.test"
        resource_record_name  = "_x.tric.test."
        resource_record_type  = "CNAME"
        resource_record_value = "_y.acm-validations.aws."
      }]
    }
  }
}

mock_provider "random" {}

variables {
  domain = "tric.test"
  # Any file: the checks only hash it.
  package = "main.tf"
}

run "module" {
  assert {
    condition     = aws_lambda_function.function["serve"].tenancy_config[0].tenant_isolation_mode == "PER_TENANT"
    error_message = "serve runs a Lambda tenant per app"
  }
  assert {
    condition     = [for f in aws_lambda_function.function : f.reserved_concurrent_executions] == [-1, 200, -1]
    error_message = "the router alone has reserved concurrency: events and serve share the account's rest"
  }
  assert {
    condition     = aws_lambda_function_url.route.function_name == "tric-route"
    error_message = "the one function URL is the router's: serve has none"
  }
  assert {
    condition     = !strcontains(aws_iam_role_policy.serve.policy, "s3:")
    error_message = "serve's role has no storage access"
  }
  assert {
    condition = (
      [for s in jsondecode(aws_iam_role_policy.serve.policy).Statement : s.Resource
      if s.Action == "lambda:InvokeFunction"]
      == ["arn:aws:lambda:us-west-2:123456789012:function:tric-events:outbox"]
    )
    error_message = "serve invokes the router's outbox alias only"
  }
  assert {
    condition = jsondecode(aws_iam_role.app.assume_role_policy).Statement == [
      { Effect = "Allow", Principal = { AWS = aws_iam_role.route.arn }, Action = "sts:AssumeRole" },
    ]
    error_message = "the app role trusts only the router's"
  }
  assert {
    condition = (
      [for s in jsondecode(aws_iam_role_policy.app.policy).Statement : s.Resource if contains(s.Action, "s3:PutObject")]
      == [["${aws_s3_bucket.store.arn}/apps/*/names/*", "${aws_s3_bucket.store.arn}/apps/*/values/*",
      "${aws_s3_bucket.store.arn}/native/*"]]
    )
    error_message = "no app session can write a release or a component, whatever policy the router gives it"
  }
  assert {
    condition = (
      [for s in jsondecode(aws_iam_role_policy.route.policy).Statement : s.Resource
      if contains(flatten([s.Action]), "s3:GetObject")]
      == ["${aws_s3_bucket.store.arn}/apps/*/current", "${aws_s3_bucket.store.arn}/ws/*"]
    )
    error_message = "the router reads releases and sockets' records only, never an app's data"
  }
  assert {
    condition = (
      [for s in jsondecode(aws_iam_role_policy.route.policy).Statement : s.Resource
      if s.Action == "execute-api:ManageConnections"]
      == [["arn:aws:execute-api:us-west-2:123456789012:a1b2c3/ws/POST/@connections/*",
      "arn:aws:execute-api:us-west-2:123456789012:a1b2c3/ws/DELETE/@connections/*"]]
    )
    error_message = "the router sends to, and closes, its own API's sockets only"
  }
  assert {
    condition = (
      jsondecode(aws_iam_role_policy.scheduler.policy).Statement[0].Resource
      == "arn:aws:lambda:us-west-2:123456789012:function:tric-events:cron"
    )
    error_message = "Scheduler invokes the events function as cron, never as outbox or ws"
  }
  assert {
    condition = (
      aws_lambda_permission.ws.function_name == "tric-events" && aws_lambda_permission.ws.qualifier == "ws"
      && aws_lambda_permission.ws.principal == "apigateway.amazonaws.com"
      && aws_lambda_permission.ws.source_arn == "arn:aws:execute-api:us-west-2:123456789012:a1b2c3/ws/*"
      && aws_apigatewayv2_integration.ws.integration_uri == aws_lambda_alias.events["ws"].invoke_arn
      && alltrue([for r in aws_apigatewayv2_route.ws : r.route_response_selection_expression == null])
    )
    error_message = "API Gateway alone invokes the ws alias, from its own stage, with no route response"
  }
  assert {
    condition = (
      aws_apigatewayv2_stage.ws.default_route_settings[0].throttling_rate_limit == 100
      && aws_apigatewayv2_stage.ws.default_route_settings[0].throttling_burst_limit == 200
    )
    error_message = "the sockets' stage is throttled"
  }
  assert {
    condition = (
      aws_lambda_function_event_invoke_config.outbox.function_name == "tric-events"
      && aws_lambda_function_event_invoke_config.outbox.qualifier == "outbox"
      && aws_lambda_function_event_invoke_config.outbox.maximum_retry_attempts == 2
      && aws_lambda_function_event_invoke_config.outbox.maximum_event_age_in_seconds == 21600
      && one(aws_lambda_function_event_invoke_config.outbox.destination_config[0].on_failure).destination
      == aws_s3_bucket.store.arn
    )
    error_message = "the outbox alias retries twice, keeps events 6 hours, then writes them to the bucket"
  }
  assert {
    condition = (
      aws_lambda_function.function["route"].environment[0].variables.TRIC_ORIGIN == random_password.origin.result
      && aws_lambda_function.function["events"].environment[0].variables.TRIC_ORIGIN == random_password.origin.result
      && flatten([for o in aws_cloudfront_distribution.apps.origin : [for h in o.custom_header : h.value]])
      == [random_password.origin.result]
      && !contains(keys(aws_lambda_function.function["serve"].environment[0].variables), "TRIC_ORIGIN")
    )
    error_message = "CloudFront sends the router's secret, with requests and sockets' openings; serve never has it"
  }
  assert {
    condition = (
      aws_s3_bucket_versioning.store.versioning_configuration[0].status == "Enabled"
      && aws_s3_bucket_lifecycle_configuration.store.rule[0].noncurrent_version_expiration[0].noncurrent_days == 1
      && aws_s3_bucket_lifecycle_configuration.store.rule[0].expiration[0].expired_object_delete_marker
      && aws_s3_bucket_lifecycle_configuration.store.rule[1].filter[0].prefix == "aws/lambda/async/"
      && aws_s3_bucket_lifecycle_configuration.store.rule[1].expiration[0].days == 14
      && aws_s3_bucket_lifecycle_configuration.store.rule[2].filter[0].prefix == "ws/"
      && aws_s3_bucket_lifecycle_configuration.store.rule[2].expiration[0].days == 1
    )
    error_message = "versioned; lifecycle expires noncurrent versions, delete markers, the dead letters and sockets"
  }
}
