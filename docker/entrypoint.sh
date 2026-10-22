#!/bin/sh
# Container entrypoint. Applies optional corporate settings, then starts the
# server. Repositories listed in VK_GIT_REPOS are cloned and registered in the
# background so a slow clone never delays startup.
set -eu

port="${PORT:-3000}"

# Corporate CA: append it to the system bundle for git, the server, Node.js
# and Python-based tools.
if [ -n "${VK_EXTRA_CA_CERTS:-}" ]; then
  if [ -r "$VK_EXTRA_CA_CERTS" ]; then
    bundle=/tmp/vk-ca-bundle.pem
    cat /etc/ssl/certs/ca-certificates.crt "$VK_EXTRA_CA_CERTS" >"$bundle"
    export SSL_CERT_FILE="$bundle" REQUESTS_CA_BUNDLE="$bundle"
    export NODE_EXTRA_CA_CERTS="$VK_EXTRA_CA_CERTS"
    # Via git config rather than env so `docker exec` shells pick it up too.
    git config --global http.sslCAInfo "$bundle"
  else
    echo "entrypoint: VK_EXTRA_CA_CERTS=$VK_EXTRA_CA_CERTS is not readable, ignoring" >&2
  fi
else
  git config --global --unset http.sslCAInfo 2>/dev/null || true
fi

# Commit identity for the developer who owns this sandbox.
if [ -n "${VK_GIT_USER_NAME:-}" ]; then
  git config --global user.name "$VK_GIT_USER_NAME"
fi
if [ -n "${VK_GIT_USER_EMAIL:-}" ]; then
  git config --global user.email "$VK_GIT_USER_EMAIL"
fi

wait_for_server() {
  i=0
  until wget -q --spider "http://127.0.0.1:${port}/api/health" 2>/dev/null; do
    i=$((i + 1))
    [ "$i" -lt 120 ] || return 1
    sleep 1
  done
}

clone_and_register_repos() {
  cloned=
  for url in $(printf '%s' "${VK_GIT_REPOS:-}" | tr ',' ' '); do
    name=$(basename "$url" .git)
    case "$name" in
      '' | *[!A-Za-z0-9._%-]*)
        echo "entrypoint: skipping $url (unsupported repository name)" >&2
        continue
        ;;
    esac
    dest="/repos/$name"
    [ -e "$dest" ] && continue

    echo "entrypoint: cloning $url into $dest"
    if git clone --quiet "$url" "$dest"; then
      cloned="$cloned $dest"
    else
      echo "entrypoint: failed to clone $url" >&2
    fi
  done

  [ -n "$cloned" ] || return 0
  if ! wait_for_server; then
    echo "entrypoint: server did not become healthy, repositories not registered" >&2
    return 0
  fi
  for dest in $cloned; do
    if wget -qO /dev/null --header='Content-Type: application/json' \
      --post-data="{\"path\":\"$dest\"}" "http://127.0.0.1:${port}/api/repos"; then
      echo "entrypoint: registered $dest"
    else
      echo "entrypoint: failed to register $dest" >&2
    fi
  done
}

if [ -n "${VK_GIT_REPOS:-}" ]; then
  clone_and_register_repos &
fi

exec /usr/local/bin/server "$@"
