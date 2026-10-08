output "store" {
  description = "TRIC_STORE for the CLI."
  value       = "s3://${aws_s3_bucket.this.bucket}"
}

output "region" {
  description = "AWS_REGION for the CLI."
  value       = var.region
}

output "apps" {
  description = "Where each app is served."
  value       = "https://<app>.${var.domain}"
}

output "function_url" {
  description = "The function without CloudFront, which routes by `X-Forwarded-Host`."
  value       = aws_lambda_function_url.this.function_url
}

output "deploy_policy" {
  description = "The policy to attach to whoever deploys."
  value       = aws_iam_policy.deploy.arn
}
