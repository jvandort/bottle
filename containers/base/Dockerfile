# bottle/base: Debian stable-slim plus the minimum every bottle needs.

# Pinned by digest so rebuilds are reproducible and the weekly upstream rebuild
# never changes the image underneath us. Bump the digest deliberately.
FROM debian:stable-slim@sha256:5bc3287b25407c965a30f38e32603dc253a3869e1b12a21ac09bfc27fd8b13ce

# The stable-slim image points apt at the "stable" suite, which silently becomes
# the next Debian release when it ships. Rewrite the suites to this image's own
# codename (e.g. trixie) so any later apt install stays on the same release.
RUN . /etc/os-release \
 && sed -i -E "s/\bstable(-updates|-security)?\b/${VERSION_CODENAME}\1/g" \
      /etc/apt/sources.list.d/debian.sources

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates \
      curl \
      fd-find \
      git \
      jq \
      less \
      openssh-client \
      openssh-server \
      procps \
      ripgrep \
      sudo \
      tini \
      tmux \
      unzip \
      xz-utils \
      vim \
 && rm -rf /var/lib/apt/lists/* \
 && ln -s /usr/bin/fdfind /usr/local/bin/fd

RUN groupadd --gid 1000 genie \
 && useradd --uid 1000 --gid genie --create-home --shell /bin/bash genie \
 # Enable passwordless sudo
 && echo 'genie ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/genie \
 && chmod 0440 /etc/sudoers.d/genie \
 && install -d -o genie -g genie /workspace

COPY sshd_config /etc/ssh/sshd_config.d/bottle.conf
# Installing openssh-server generates host keys. Delete the defaults to prevent
# each container from sharing the same key. We generate a fresh key on startup.
RUN rm -f /etc/ssh/ssh_host_*

COPY gitconfig /etc/gitconfig
COPY tmux.conf /etc/tmux.conf
COPY entrypoint.sh /usr/local/bin/bottle-entrypoint

ENV LANG=C.UTF-8 \
    EDITOR=vim

# Default directory for `exec` sessions: the project.
WORKDIR /workspace

LABEL org.opencontainers.image.title="bottle/base"

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/bottle-entrypoint"]
