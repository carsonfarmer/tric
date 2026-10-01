variable "region" {
  description = "AWS region. us-west-2 is where the spike measures."
  type        = string
  default     = "us-west-2"
}

variable "name" {
  description = "Prefix for every resource name."
  type        = string
  default     = "spinit-spike"
}

variable "memory_sizes" {
  description = "One function per memory size (MB). Lambda gives CPU in proportion to memory, with a full vCPU at 1769 MB."
  type        = list(number)
  default     = [128, 512, 1769]
}

variable "zip_path" {
  description = "Zip holding the host binary as `bootstrap`; built by lambda/package.sh."
  type        = string
  default     = "../out/spinit-host.zip"
}

variable "lwa_layer_version" {
  description = "Version of the Lambda Web Adapter arm64 layer (account 753240598075, published by AWS)."
  type        = number
  default     = 30
}

variable "component" {
  description = "SPINIT_COMPONENT: the app to serve, as sha256:<hex> of the blob in the bucket. Default is the Rust p3 test component; bench/cloud.sh swaps it per run."
  type        = string
  default     = "sha256:47752d4cb024f8c7b10f23b3b6cb06a4e688be6b7d2fe5b195d67569ed3e5001"
}

variable "precompiled" {
  description = "SPINIT_PRECOMPILED: 1 looks for a precompiled artifact in the bucket before compiling the blob."
  type        = string
  default     = "1"
}

variable "eager" {
  description = "SPINIT_EAGER: 1 loads the component before the host listens (runs in Lambda's init phase), 0 on the first request."
  type        = string
  default     = "0"
}

variable "allocator" {
  description = "SPINIT_ALLOCATOR: default or pooling."
  type        = string
  default     = "default"
}

variable "timeout_seconds" {
  description = "Function timeout. Generous, because a blob-compile cold start on 128 MB is minutes, not milliseconds."
  type        = number
  default     = 300
}
