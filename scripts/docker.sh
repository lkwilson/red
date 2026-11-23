#!/usr/bin/env bash
# Sourced helper; use the daemon's default BuildKit builder to retain layer cache
# between jobs. No named builders, shared default-context changes, or cache tags.
source "$(dirname "${BASH_SOURCE[0]}")/env.sh"
if [[ -n ${IMAGE_DOCKER_ENDPOINT:-} ]]; then
  export DOCKER_HOST=$IMAGE_DOCKER_ENDPOINT
  unset DOCKER_CONTEXT
fi
image_args=(--build-arg "APP_NAME=$APP_NAME")
for key in RUST_IMAGE XX_IMAGE CARGO_BUILD_JOBS SCCACHE_VERSION NODE_IMAGE NGINX_IMAGE PYTHON_IMAGE UV_IMAGE UI_DIR UI_OUTPUT_DIR NPM_BUILD_SCRIPT APP_VERSION_ENV; do
  if [[ -n ${!key:-} ]]; then image_args+=(--build-arg "$key=${!key}"); fi
done
if [[ $APP_VARIANT == rust || $APP_VARIANT == rust-cross ]]; then
  : "${AWS_ACCESS_KEY_ID:?CI must provide the sccache access key}"
  : "${AWS_SECRET_ACCESS_KEY:?CI must provide the sccache secret key}"
  for key in SCCACHE_BUCKET SCCACHE_ENDPOINT SCCACHE_REGION SCCACHE_S3_USE_SSL; do
    image_args+=(--build-arg "$key=${!key:?}")
  done
  # The Dockerfile adds profile, builder images, builder/target and compiler identity.
  image_args+=(--build-arg "SCCACHE_S3_KEY_PREFIX=$APP_NAME"
    --secret id=sccache_aws_access_key_id,env=AWS_ACCESS_KEY_ID
    --secret id=sccache_aws_secret_access_key,env=AWS_SECRET_ACCESS_KEY)
fi
