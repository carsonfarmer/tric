output "store" {
  description = "TRIC_STORE for the CLI."
  value       = local.env.TRIC_STORE
}

output "native" {
  description = "TRIC_NATIVE for the CLI, so `publish` waits for native code."
  value       = local.env.TRIC_NATIVE
}

output "function_url" {
  description = "The serving function without CloudFront, which routes by `X-Forwarded-Host`."
  value       = aws_lambda_function_url.serve.function_url
}

output "team_roles" {
  value = { for t, r in aws_iam_role.team : t => r.arn }
}
