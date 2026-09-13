$ErrorActionPreference = 'Stop'
[Console]::Write("The authenticity of host 'example.test' can't be established. Are you sure you want to continue connecting (yes/no/[fingerprint])?")
[void][Console]::ReadLine()
[Console]::Write("Enter passphrase for key 'id_ed25519':")
[void][Console]::ReadLine()
exit 0
