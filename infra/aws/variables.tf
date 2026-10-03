variable "name" {
  description = "Prefix of every resource's name."
  type        = string
  default     = "torpor"
  validation {
    condition     = can(regex("^[a-z][a-z0-9-]{0,19}$", var.name))
    error_message = "A name is 1 to 20 of a-z, 0-9 and -, so the buckets' names fit."
  }
}

variable "region" {
  type    = string
  default = "us-west-2"
}

variable "domain" {
  description = "A Route 53 zone of this account. Each app is served at <app>.<domain>."
  type        = string
}

variable "teams" {
  description = "Teams, each with a role that may publish and release the apps named <team>-…. The roles trust this account, so its own policies say who may assume each."
  type        = set(string)
  default     = []
  validation {
    condition     = alltrue([for t in var.teams : can(regex("^[a-z0-9]+$", t))])
    error_message = "Team names are a-z and 0-9, with no -, or team `a` would own team `a-b`'s apps."
  }
}

variable "memory" {
  description = "The serving function's memory in MB. 1769 is one full vCPU."
  type        = number
  default     = 1769
  validation {
    condition     = var.memory >= 512
    error_message = "At least 512 MB, so an app's 256 MiB store always fits."
  }
}

variable "storage" {
  description = "The serving function's /tmp in MB, which holds the native code of every app a host has loaded."
  type        = number
  default     = 512
}

variable "concurrency" {
  description = "The most serving functions that run at once, which caps what a flood of requests can cost. -1 for no cap."
  type        = number
  default     = 20
}

variable "budget" {
  description = "The account's monthly budget in USD, which alerts at 80%."
  type        = number
  default     = 5
}

variable "alert_emails" {
  description = "Who the budget alerts."
  type        = list(string)
  validation {
    condition     = length(var.alert_emails) > 0
    error_message = "A budget alert needs someone to alert."
  }
}

variable "log_days" {
  type    = number
  default = 7
}

variable "log_filter" {
  description = "RUST_LOG for both functions, like `info` or `warn,[request{app=NAME}]=info`."
  type        = string
  default     = "warn"
}

variable "force_destroy" {
  description = "Whether `tofu destroy` deletes the buckets with everything in them. Apply it before destroying."
  type        = bool
  default     = false
}
