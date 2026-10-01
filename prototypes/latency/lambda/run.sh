#!/bin/sh
# Lambda "handler" for the zip: starts the host; the Lambda Web Adapter extension does the Runtime API work and proxies to it.
exec /var/task/spinit-host
