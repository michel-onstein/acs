# A throwaway ssh host for end-to-end checks (scripts/e2e_ssh.sh): sshd,
# users `dev`, `solo` and `copier` that only accept the key passed in at run
# time, and a few TUIs to look at. `solo` never gets an install of its own: it
# is the user the system-wide install (`/usr/local/lib/acs`) is checked as
# (acs-1gy). `copier` is first reached by `acscp`, so the copy's own
# first-contact install is checked on an account `acs` never touched
# (acs-hofu.8).
#
# `AllowTcpForwarding yes` is a test fixture, not a recommendation: Alpine's
# openssh ships the line as `no`, which would make the `-L` case (acs-8kv)
# untestable. It says nothing about what acs installs or asks for — acs
# never touches a host's sshd configuration.
FROM alpine:3
RUN apk add --no-cache openssh-server vim less \
 && ssh-keygen -A \
 && adduser -D -s /bin/sh dev \
 && adduser -D -s /bin/sh solo \
 && adduser -D -s /bin/sh copier \
 && sed -i -e 's/^dev:!/dev:*/' -e 's/^solo:!/solo:*/' -e 's/^copier:!/copier:*/' /etc/shadow \
 && sed -i 's/^#\?PasswordAuthentication.*/PasswordAuthentication no/' /etc/ssh/sshd_config \
 && sed -i 's/^#\?AllowTcpForwarding.*/AllowTcpForwarding yes/' /etc/ssh/sshd_config
COPY entrypoint.sh /entrypoint.sh
ENTRYPOINT ["/bin/sh", "/entrypoint.sh"]
