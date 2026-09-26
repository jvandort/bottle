# bottle/tools: a minimal environment for doing useful work.
FROM bottle/base:latest

# Builds have no terminal; stop debconf trying (and warning about) interactive prompts.
# ARG, not ENV, so it doesn't leak into bottles.
ARG DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      # TLS root certificates; without them curl, git, and apt over https fail.
      ca-certificates \
      curl \
      fd-find \
      git \
      jq \
      less \
      # For git over ssh.
      openssh-client \
      # ps, top, kill.
      procps \
      ripgrep \
      tmux \
      unzip \
      vim \
      xz-utils \
 && rm -rf /var/lib/apt/lists/* \
 # Debian names the binary fdfind; expose it under its upstream name.
 && ln -s /usr/bin/fdfind /usr/local/bin/fd

COPY gitconfig /etc/gitconfig
COPY tmux.conf /etc/tmux.conf

# ENV reaches `container exec`; SSH sessions only see /etc/environment (via PAM).
ENV EDITOR=vim
RUN printf '%s\n' "EDITOR=$EDITOR" >> /etc/environment

LABEL org.opencontainers.image.title="bottle/tools"
