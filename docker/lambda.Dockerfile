# Lambda, locally: provided.al2023, whose Runtime Interface Emulator takes invocations at
# /2015-03-31/functions/function/invocations; the adapter, where its layer puts it; and dist/tric.zip, where Lambda
# unpacks a function's code. The adapter's version is the one the layer in infra/aws/main.tf has.
FROM public.ecr.aws/awsguru/aws-lambda-adapter:1.1.0 AS adapter
FROM public.ecr.aws/amazonlinux/amazonlinux:2023 AS code
COPY tric.zip /
RUN python3 -m zipfile -e /tric.zip /task && chmod +x /task/bootstrap /task/tric
FROM public.ecr.aws/lambda/provided:al2023
COPY --from=adapter /lambda-adapter /opt/extensions/lambda-adapter
COPY --from=code /task/ /var/task/
CMD ["serve"]
