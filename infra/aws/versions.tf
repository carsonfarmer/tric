terraform {
  required_version = ">= 1.10"
  required_providers {
    aws = { source = "hashicorp/aws", version = "~> 6.28" } # from 6.28, a public Function URL gets both its permissions
  }
}

# Every resource is tagged `torpor = <name>`, so what an install left behind is easy to find.
provider "aws" {
  region = var.region
  default_tags { tags = { torpor = var.name } }
}

# CloudFront takes certificates only from us-east-1.
provider "aws" {
  alias  = "us_east_1"
  region = "us-east-1"
  default_tags { tags = { torpor = var.name } }
}
