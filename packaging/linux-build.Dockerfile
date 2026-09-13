# Build with: docker build -f packaging/linux-build.Dockerfile -t devmanager-linux-build .
# Rust 1.94.0 is supplied by the caller; no user profiles enter the build image.
FROM ubuntu:24.04@sha256:33ceb71981b602c1a7443a53469e4dba065f7503eab3078a2d7a57a2ab987517
ENV DEBIAN_FRONTEND=noninteractive
COPY packaging/install-linux-build-dependencies.sh /tmp/install-linux-build-dependencies.sh
RUN bash /tmp/install-linux-build-dependencies.sh && rm -rf /var/lib/apt/lists/* /tmp/install-linux-build-dependencies.sh
