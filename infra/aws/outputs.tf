output "store" {
  description = "TORPOR_STORE for the CLI."
  value       = "s3://${aws_s3_bucket.this["app"].bucket}"
}

output "native" {
  description = "TORPOR_NATIVE for the CLI, so `publish` waits for native code."
  value       = "s3://${aws_s3_bucket.this["native"].bucket}"
}

output "function_url" {
  description = "The serving function without CloudFront, which routes by `X-Forwarded-Host`."
  value       = aws_lambda_function_url.serve.function_url
}

output "team_roles" {
  value = { for t, r in aws_iam_role.team : t => r.arn }
}
