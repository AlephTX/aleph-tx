#!/usr/bin/env bash
# Run as the SSH development user on oasis, with sudo access.
set -euo pipefail

sudo -n apt-get update
sudo -n env DEBIAN_FRONTEND=noninteractive NEEDRESTART_MODE=l apt-get -o DPkg::Lock::Timeout=120 -y --no-remove upgrade
sudo -n env DEBIAN_FRONTEND=noninteractive NEEDRESTART_MODE=l apt-get -o DPkg::Lock::Timeout=120 -y --no-remove install \
  build-essential ca-certificates curl git golang-go jq libssl-dev pkg-config \
  python3 ripgrep rsync rustup tmux unzip zsh

omz="$HOME/.oh-my-zsh"
if [[ ! -d "$omz" ]]; then
  git clone --depth 1 https://github.com/ohmyzsh/ohmyzsh.git "$omz"
fi
test -r "$omz/oh-my-zsh.sh"
zshrc="$HOME/.zshrc"
if ! grep -Eq 'oh-my-zsh\.sh' "$zshrc" 2>/dev/null; then
  if [[ -e "$zshrc" ]]; then
    cp -p -- "$zshrc" "$zshrc.pre-aleph.$(date +%Y%m%d%H%M%S)"
  fi
  cat >> "$zshrc" <<'ZSHRC'

# Oh My Zsh
export ZSH="$HOME/.oh-my-zsh"
ZSH_THEME="robbyrussell"
plugins=(git sudo)
zstyle ':omz:update' mode reminder
source "$ZSH/oh-my-zsh.sh"
ZSHRC
fi
if [[ "$(getent passwd "$(id -un)" | cut -d: -f7)" != /usr/bin/zsh ]]; then
  if getent -s files passwd "$(id -un)" >/dev/null; then
    sudo -n chsh -s /usr/bin/zsh "$(id -un)"
  elif ! grep -Fq '# AlephTX: enter Zsh on interactive OS Login sessions' "$HOME/.bashrc" 2>/dev/null; then
    # OS Login supplies the account through NSS, so chsh cannot update it.
    if [[ -e "$HOME/.bashrc" ]]; then
      cp -p -- "$HOME/.bashrc" "$HOME/.bashrc.pre-aleph.$(date +%Y%m%d%H%M%S)"
    fi
    cat >> "$HOME/.bashrc" <<'BASHRC'

# AlephTX: enter Zsh on interactive OS Login sessions
case $- in
  *i*) if [ -z "${ALEPH_ZSH_STARTED:-}" ] && [ -x /usr/bin/zsh ]; then
         export ALEPH_ZSH_STARTED=1
         exec /usr/bin/zsh -l
       fi ;;
esac
BASHRC
  fi
fi
zsh -n "$zshrc"
rustup toolchain install 1.95.0 --profile minimal --component clippy --component rustfmt
if ! rustup show active-toolchain >/dev/null 2>&1; then
  rustup default 1.95.0
fi
printf '\nDevelopment tools:\n'
go version
rustup run 1.95.0 rustc --version
zsh --version
printf 'Login shell: %s\n' "$(getent passwd "$(id -un)" | cut -d: -f7)"
