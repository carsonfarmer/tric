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
}

override_resource {
  target = aws_iam_role.route
  values = { arn = "arn:aws:iam::123456789012:role/tric-route" }
}

override_resource {
  target = aws_iam_role.app
  values = { arn = "arn:aws:iam::123456789012:role/tric-app" }
}

override_resource {
  target = aws_iam_role.serve
  values = { arn = "arn:aws:iam::123456789012:role/tric-serve" }
}

override_resource {
  target = aws_iam_role.scheduler
  values = { arn = "arn:aws:iam::123456789012:role/tric-scheduler" }
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
      [for s in jsondecode(aws_iam_role_policy.route.policy).Statement : s.Resource if s.Action == "s3:GetObject"]
      == ["${aws_s3_bucket.store.arn}/apps/*/current"]
    )
    error_message = "the router reads releases only, never an app's data"
  }
  assert {
    condition = (
      jsondecode(aws_iam_role_policy.scheduler.policy).Statement[0].Resource
      == "arn:aws:lambda:us-west-2:123456789012:function:tric-events"
    )
    error_message = "Scheduler invokes the events function unqualified, never as outbox"
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
      && flatten([for o in aws_cloudfront_distribution.apps.origin : [for h in o.custom_header : h.value]])
      == [random_password.origin.result]
      && !contains(keys(aws_lambda_function.function["events"].environment[0].variables), "TRIC_ORIGIN")
    )
    error_message = "CloudFront sends the route function's secret; the events function has none, so takes only events"
  }
  assert {
    condition = (
      aws_s3_bucket_versioning.store.versioning_configuration[0].status == "Enabled"
      && aws_s3_bucket_lifecycle_configuration.store.rule[0].noncurrent_version_expiration[0].noncurrent_days == 1
      && aws_s3_bucket_lifecycle_configuration.store.rule[0].expiration[0].expired_object_delete_marker
      && aws_s3_bucket_lifecycle_configuration.store.rule[1].filter[0].prefix == "aws/lambda/async/"
      && aws_s3_bucket_lifecycle_configuration.store.rule[1].expiration[0].days == 14
    )
    error_message = "versioned; lifecycle expires noncurrent versions, delete markers and the dead letters"
  }
}
