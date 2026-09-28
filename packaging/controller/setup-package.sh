#!/bin/sh
set -eu
# Never change ownership of existing state, replace local configuration,
# enable the service, or restart running builds.
pier_user=pier-controller
pier_state=/var/lib/pier-controller
if ! getent group "$pier_user" >/dev/null; then groupadd --system "$pier_user"; fi
if getent passwd "$pier_user" >/dev/null; then
    [ "$(id -u "$pier_user")" -ne 0 ] &&
    [ "$(getent passwd "$pier_user" | cut -d: -f6)" = "$pier_state" ] &&
    [ "$(id -g "$pier_user")" = "$(getent group "$pier_user" | cut -d: -f3)" ] || {
        echo 'Existing pier-controller account is incompatible; inspect it before installing.' >&2
        exit 1
    }
else
    useradd --system --gid "$pier_user" --home-dir "$pier_state" --no-create-home --shell "$(command -v nologin)" "$pier_user"
fi
[ ! -L "$pier_state" ] || { echo 'Controller state directory must not be a symlink.' >&2; exit 1; }
if [ ! -e "$pier_state" ]; then
    install -d -o "$pier_user" -g "$pier_user" -m 0700 "$pier_state"
fi
if [ ! -e /etc/pier ]; then install -d -m 0755 /etc/pier; fi
if [ ! -e /etc/pier/controller.yml ] && [ ! -L /etc/pier/controller.yml ]; then
    install -o root -g "$pier_user" -m 0640 /usr/lib/pier-controller/controller.yml /etc/pier/controller.yml
fi
echo 'Configure an HTTPS reverse proxy, then: sudo systemctl enable --now pier-controller'
echo 'Open /init in the browser; configure the repository and synchronize it manually.'
echo 'Upgrades do not restart controller. Apply them with: sudo systemctl restart pier-controller'
