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

variable "serve" {
  description = "The serving function."
  type = object({
    memory  = optional(number, 1769) # MB, where 1769 is one full vCPU
    storage = optional(number, 512)  # MB of /tmp, which holds the native code of every app a host has loaded
  })
  default = {}
  validation {
    condition     = var.serve.memory >= 512
    error_message = "At least 512 MB of memory, so an app's 256 MiB store always fits."
  }
}

variable "concurrency" {
  description = "How many of each function may run at once, reserved from the account's quota, of which Lambda keeps 100 unreserved. `serve`'s caps what a flood of requests can cost; `compile`'s queues a burst of markers, so the later ones find the native code made. -1 reserves none, for an account whose quota is too small."
  type        = object({ serve = optional(number, 20), compile = optional(number, 2) })
  default     = {}
}

variable "budget" {
  description = "The account's monthly budget in USD, and who is alerted at 80% of it."
  type        = object({ usd = optional(number, 5), emails = list(string) })
}

variable "logs" {
  description = "How many days both functions' logs are kept, and their RUST_LOG, like `info` or `warn,[request{app=NAME}]=info`."
  type        = object({ days = optional(number, 7), filter = optional(string, "warn") })
  default     = {}
}

variable "force_destroy" {
  description = "Whether `tofu destroy` deletes the buckets with everything in them. Apply it before destroying."
  type        = bool
  default     = false
}
