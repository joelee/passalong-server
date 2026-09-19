# syntax=docker/dockerfile:1
# A throwaway host with systemd as PID 1, for scripts/test-service.sh: the one
# place where `passalong-server service install` runs for real, as root. The
# same Debian as the server's image, so that image's binary runs here.
FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends systemd systemd-sysv sudo curl ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && systemctl mask getty.target console-getty.service systemd-logind.service systemd-sysctl.service \
    # Kernel settings are the host's, not a container's: `kernel.core_pattern`
    # is one kernel-wide value, and a container allowed to write it changes it
    # for the machine it runs on. This image sets none, and the script that
    # runs it gives it no way to.
    && rm -f /usr/lib/sysctl.d/*.conf /etc/sysctl.d/*.conf /etc/sysctl.conf
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
