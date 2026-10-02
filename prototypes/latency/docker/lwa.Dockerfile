# The host behind the Lambda Web Adapter, run by the Lambda Runtime Interface Emulator that ships in the base image.
# Build context: prototypes/latency (needs out/spinit-host from docker/build-host.sh).
# Same layout as the real function: the binary is /var/task/bootstrap and the adapter is an extension in /opt/extensions.
FROM public.ecr.aws/lambda/provided:al2023
COPY --from=public.ecr.aws/awsguru/aws-lambda-adapter:1.1.0 /lambda-adapter /opt/extensions/lambda-adapter
COPY out/spinit-host /var/task/bootstrap
# RIE itself listens on 8080, so the app uses 3000 here (in Lambda it is 8080).
ENV AWS_LWA_PORT=3000 AWS_LWA_READINESS_CHECK_PATH=/__ready SPINIT_ADDR=127.0.0.1:3000
CMD ["unused-handler-name"]
