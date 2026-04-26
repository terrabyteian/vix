# Fixture: bare HCL — exercises HCL highlighting.

service "api" {
  enabled = true
  port    = 8080

  endpoints = ["/health", "/v1/users", "/v1/items"]

  rate_limits = {
    default  = 100
    burst    = 250
    per_user = 30
  }

  tls {
    cert = "/etc/api/tls.crt"
    key  = "/etc/api/tls.key"
  }
}

service "worker" {
  enabled  = false
  replicas = 0

  env = {
    LOG_LEVEL = "debug"
    QUEUE_URL = "amqp://${var.broker_host}:5672"
  }

  message = <<-EOT
    Multi-line
    heredoc
    body for ${service.api.port}.
  EOT
}

variable "broker_host" {
  type    = string
  default = "rabbit.local"
}

# Operators, conditionals, function calls, splat, for-expression
locals {
  total       = 1 + 2 * 3 - 4 / 2
  greeting    = upper("hello, ${var.broker_host}!")
  has_workers = service.worker.enabled && service.worker.replicas > 0
  ports       = [for s in [service.api, service.worker] : s.port if s.enabled]
}
