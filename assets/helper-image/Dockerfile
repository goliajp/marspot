# The agent command lines for a helper started with isolation
# "container".  Built on first use; the tag carries the hash of this file.
# Every version is named, so that the same file is the same image.
FROM node:22-slim@sha256:e9f565d32859f0cbfb52125eafe8be39390f24dac302dc3cdf566e8ba3ab21ee

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl git procps python3 ripgrep \
 && rm -rf /var/lib/apt/lists/*

RUN npm install -g --no-fund --no-audit \
      @anthropic-ai/claude-code@2.1.330 @google/gemini-cli@0.70.0

# the vendor's installer puts kimi under ~/.kimi-code, where an account's
# home is mounted at run time
USER node
RUN curl -fsSL -o /tmp/kimi-install.sh https://code.kimi.com/kimi-code/install.sh \
 && KIMI_VERSION=2.7.2 KIMI_NO_MODIFY_PATH=1 bash /tmp/kimi-install.sh \
 && rm /tmp/kimi-install.sh
USER root
RUN mv /home/node/.kimi-code/bin/kimi /usr/local/bin/kimi \
 && rm -rf /home/node/.kimi-code

# not root: claude refuses to run without prompts as root
RUN install -d -o node -g node /work /marspot /claude
USER node
WORKDIR /work
ENV DISABLE_AUTOUPDATER=1
RUN claude --version && gemini --version && kimi --version \
 && rm -rf /home/node/.claude /home/node/.claude.json /home/node/.gemini /home/node/.kimi-code
