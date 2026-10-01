# Latency spike, cloud half: one bucket, one table, and one function per memory size, all running the same zip.
# Nothing here is for production; `tofu destroy` removes everything (the bucket has force_destroy).

data "aws_caller_identity" "current" {}

locals {
  functions = { for m in var.memory_sizes : tostring(m) => m }
  # The Lambda Web Adapter layer: the extension at /opt/extensions/lambda-adapter and the wrapper at /opt/bootstrap.
  lwa_layer = "arn:aws:lambda:${var.region}:753240598075:layer:LambdaAdapterLayerArm64:${var.lwa_layer_version}"
}

resource "aws_s3_bucket" "this" {
  bucket        = "${var.name}-${data.aws_caller_identity.current.account_id}-${var.region}"
  force_destroy = true
}

resource "aws_s3_bucket_public_access_block" "this" {
  bucket                  = aws_s3_bucket.this.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_dynamodb_table" "this" {
  name         = var.name
  billing_mode = "PAY_PER_REQUEST"
  hash_key     = "pk"

  attribute {
    name = "pk"
    type = "S"
  }
}

resource "aws_cloudwatch_log_group" "fn" {
  for_each          = local.functions
  name              = "/aws/lambda/${var.name}-${each.key}"
  retention_in_days = 1
}

resource "aws_iam_role" "fn" {
  name = "${var.name}-fn"

  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "lambda.amazonaws.com" }
      Action    = "sts:AssumeRole"
    }]
  })
}

resource "aws_iam_role_policy" "fn" {
  name = "spike"
  role = aws_iam_role.fn.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Sid      = "BucketObjects"
        Effect   = "Allow"
        Action   = ["s3:GetObject", "s3:PutObject"]
        Resource = "${aws_s3_bucket.this.arn}/*"
      },
      {
        # Without ListBucket, S3 answers 403 instead of 404 for a missing key, which would break the precompiled-artifact lookup.
        Sid      = "BucketMissingKeysAre404"
        Effect   = "Allow"
        Action   = "s3:ListBucket"
        Resource = aws_s3_bucket.this.arn
      },
      {
        Sid      = "Table"
        Effect   = "Allow"
        Action   = ["dynamodb:GetItem", "dynamodb:PutItem"]
        Resource = aws_dynamodb_table.this.arn
      },
      {
        Sid      = "Logs"
        Effect   = "Allow"
        Action   = ["logs:CreateLogStream", "logs:PutLogEvents"]
        Resource = [for g in aws_cloudwatch_log_group.fn : "${g.arn}:*"]
      },
    ]
  })
}

resource "aws_lambda_function" "fn" {
  for_each = local.functions

  function_name    = "${var.name}-${each.key}"
  role             = aws_iam_role.fn.arn
  runtime          = "provided.al2023"
  architectures    = ["arm64"]
  handler          = "bootstrap" # ignored by provided runtimes, required for zips
  filename         = var.zip_path
  source_code_hash = filebase64sha256(var.zip_path)
  memory_size      = each.value
  timeout          = var.timeout_seconds
  layers           = [local.lwa_layer]

  logging_config {
    log_format = "Text"
    log_group  = aws_cloudwatch_log_group.fn[each.key].name
  }

  environment {
    variables = {
      AWS_LAMBDA_EXEC_WRAPPER      = "/opt/bootstrap"
      AWS_LWA_PORT                 = "8080"
      AWS_LWA_READINESS_CHECK_PATH = "/__ready"
      SPINIT_ADDR                  = "127.0.0.1:8080"
      SPINIT_BUCKET                = aws_s3_bucket.this.bucket
      SPINIT_TABLE                 = aws_dynamodb_table.this.name
      SPINIT_COMPONENT             = var.component
      SPINIT_PRECOMPILED           = var.precompiled
      SPINIT_EAGER                 = var.eager
      SPINIT_ALLOCATOR             = var.allocator
      SPINIT_CACHE_DIR             = "/tmp/spinit-cache"
    }
  }

  # bench/cloud.sh changes environment variables to force cold starts and to switch component or mode; do not fight it.
  lifecycle {
    ignore_changes = [environment]
  }

  depends_on = [aws_iam_role_policy.fn]
}

# AWS_IAM: callers sign requests with SigV4 (curl --aws-sigv4) and need lambda:InvokeFunctionUrl and lambda:InvokeFunction.
resource "aws_lambda_function_url" "fn" {
  for_each           = local.functions
  function_name      = aws_lambda_function.fn[each.key].function_name
  authorization_type = "AWS_IAM"
}
