# A throwaway ssh host for end-to-end checks (scripts/e2e_ssh.sh): sshd,
# a user `dev` that only accepts the key passed in at run time, and a few
# TUIs to look at.
FROM alpine:3
RUN apk add --no-cache openssh-server vim less \
 && ssh-keygen -A \
 && adduser -D -s /bin/sh dev \
 && sed -i 's/^dev:!/dev:*/' /etc/shadow \
 && sed -i 's/^#\?PasswordAuthentication.*/PasswordAuthentication no/' /etc/ssh/sshd_config
COPY entrypoint.sh /entrypoint.sh
ENTRYPOINT ["/bin/sh", "/entrypoint.sh"]
