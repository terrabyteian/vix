# Fixture: Terraform / OpenTofu — exercises HCL highlighting.

terraform {
  required_version = ">= 1.5"

  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.0"
    }
  }
}

provider "aws" {
  region = var.region
}

variable "region" {
  type        = string
  default     = "us-east-1"
  description = "AWS region to deploy into."
}

variable "instance_count" {
  type    = number
  default = 3
}

variable "tags" {
  type    = map(string)
  default = {
    Project = "vix-demo"
    Owner   = "ada"
  }
}

locals {
  name_prefix = "vix-${terraform.workspace}"
  enabled     = var.instance_count > 0 && var.region != ""
}

data "aws_ami" "ubuntu" {
  most_recent = true
  owners      = ["099720109477"]

  filter {
    name   = "name"
    values = ["ubuntu/images/hvm-ssd/ubuntu-jammy-22.04-amd64-server-*"]
  }
}

resource "aws_instance" "web" {
  count = local.enabled ? var.instance_count : 0

  ami           = data.aws_ami.ubuntu.id
  instance_type = "t3.micro"

  tags = merge(var.tags, {
    Name = "${local.name_prefix}-${count.index}"
  })

  user_data = <<-EOT
    #!/bin/bash
    echo "hello from ${local.name_prefix} #${count.index}" > /tmp/greeting
  EOT
}

output "instance_ids" {
  value       = [for i in aws_instance.web : i.id]
  description = "All instance IDs."
}
