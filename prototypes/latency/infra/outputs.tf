output "region" {
  value = var.region
}

output "bucket" {
  value = aws_s3_bucket.this.bucket
}

output "table" {
  value = aws_dynamodb_table.this.name
}

output "function_names" {
  description = "Function name by memory size (MB)."
  value       = { for k, f in aws_lambda_function.fn : k => f.function_name }
}

output "function_urls" {
  description = "Function URL by memory size (MB). Calls must be SigV4-signed for service lambda."
  value       = { for k, u in aws_lambda_function_url.fn : k => u.function_url }
}

output "log_groups" {
  value = { for k, g in aws_cloudwatch_log_group.fn : k => g.name }
}
