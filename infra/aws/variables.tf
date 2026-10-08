variable "name" {
  description = "The install's name, which prefixes its resources' names, so an account has one install of each name."
  type        = string
  default     = "tric"
  validation {
    condition     = can(regex("^[a-z][a-z0-9-]{0,19}$", var.name))
    error_message = "A name is 1 to 20 of a-z, 0-9 and -, starting with a letter, so the bucket's name fits."
  }
}

variable "region" {
  type    = string
  default = "us-west-2"
}

variable "domain" {
  description = "A public Route 53 zone of this account. Each app is served at <app>.<domain>."
  type        = string
}

variable "memory" {
  description = "The function's memory in MB, where 1769 is one full vCPU. At least 512, so an app's 256 MiB fits."
  type        = number
  default     = 1769
  validation {
    condition     = var.memory >= 512
    error_message = "At least 512 MB."
  }
}

variable "concurrency" {
  # Lambda keeps 100 of the account's quota unreserved, and a new account's may be only 10, so the default is none.
  description = "How many instances of the function may run at once, reserved from the account's quota, or -1."
  type        = number
  default     = -1
}

variable "budget" {
  description = "The account's monthly budget in USD, and who is emailed when its spend passes 80% of it."
  type        = object({ usd = optional(number, 5), emails = list(string) })
}

variable "logs" {
  description = "How many days the function's logs are kept, and its RUST_LOG, like `warn,tric=info` or `debug`."
  type        = object({ days = optional(number, 7), filter = optional(string, "warn,tric=info") })
  default     = {}
}

variable "force_destroy" {
  description = "Whether `tofu destroy` deletes the bucket with everything in it. Apply it before destroying."
  type        = bool
  default     = false
}
