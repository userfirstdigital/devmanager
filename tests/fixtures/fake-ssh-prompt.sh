#!/bin/sh
printf "%s" "The authenticity of host 'example.test' can't be established. Are you sure you want to continue connecting (yes/no/[fingerprint])?"
IFS= read -r _answer
printf "%s" "Enter passphrase for key 'id_ed25519':"
IFS= read -r _passphrase
exit 0
