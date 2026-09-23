# A throwaway ssh host for end-to-end checks (scripts/e2e_ssh.sh): sshd,
# a user `dev` that only accepts the key passed in at run time, and a few
# TUIs to look at.
#
# `AllowTcpForwarding yes` is a test fixture, not a recommendation: Alpine's
# openssh ships the line as `no`, which would make the `-L` case (acs-8kv)
# untestable. It says nothing about what acs installs or asks for — acs
# never touches a host's sshd configuration.
FROM alpine:3
RUN apk add --no-cache openssh-server vim less \
 && ssh-keygen -A \
 && adduser -D -s /bin/sh dev \
 && sed -i 's/^dev:!/dev:*/' /etc/shadow \
 && sed -i 's/^#\?PasswordAuthentication.*/PasswordAuthentication no/' /etc/ssh/sshd_config \
 && sed -i 's/^#\?AllowTcpForwarding.*/AllowTcpForwarding yes/' /etc/ssh/sshd_config
COPY entrypoint.sh /entrypoint.sh
ENTRYPOINT ["/bin/sh", "/entrypoint.sh"]
