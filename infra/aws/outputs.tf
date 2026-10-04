output "store" {
  description = "TORPOR_STORE for the CLI."
  value       = local.env.TORPOR_STORE
}

output "native" {
  description = "TORPOR_NATIVE for the CLI, so `publish` waits for native code."
  value       = local.env.TORPOR_NATIVE
}

output "function_url" {
  description = "The serving function without CloudFront, which routes by `X-Forwarded-Host`."
  value       = aws_lambda_function_url.serve.function_url
}

output "team_roles" {
  value = { for t, r in aws_iam_role.team : t => r.arn }
}
