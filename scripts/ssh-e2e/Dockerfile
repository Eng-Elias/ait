# The throwaway sshd for the AIT_E2E_SSH-gated e2e (Spec R09-T8).
# scripts/ssh-e2e.sh builds this per run with a generated
# authorized_keys, publishes port 22 on an ephemeral loopback port and
# removes the container afterwards. Nothing here is reused across runs.
FROM alpine:3.20

# `adduser -D` leaves `testuser:!:` in /etc/shadow — a LOCKED account,
# and Alpine's openssh is built without PAM so sshd's own shadow check
# denies pubkey auth for it ("Permission denied (publickey)"). `*` is
# the service-account value: no password login, account usable.
# VERBOSE so `docker logs` names the auth-denial reason.
RUN apk add --no-cache openssh bash \
 && adduser -D -s /bin/bash testuser \
 && sed -i 's/^testuser:!:/testuser:*:/' /etc/shadow \
 && mkdir -p /home/testuser/.ssh /run/sshd \
 && chmod 700 /home/testuser/.ssh \
 && ssh-keygen -A \
 && printf 'PubkeyAuthentication yes\nPasswordAuthentication no\nLogLevel VERBOSE\n' \
      >> /etc/ssh/sshd_config

# Baked per run by scripts/ssh-e2e.sh — a generated one-shot keypair.
COPY authorized_keys /home/testuser/.ssh/authorized_keys
RUN chown -R testuser:testuser /home/testuser/.ssh \
 && chmod 600 /home/testuser/.ssh/authorized_keys

EXPOSE 22
CMD ["/usr/sbin/sshd", "-D", "-e"]
