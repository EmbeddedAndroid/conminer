#!/bin/sh
# Read-only capture of every device store's recent epoch shape.
# Runs ON a conminer node, inside a throwaway container, touching no board.
DOCKER=docker
$DOCKER ps >/dev/null 2>&1 || DOCKER="sudo docker"
$DOCKER run --rm -v conminer-data:/d alpine sh -c '
  apk add -q sqlite 2>/dev/null
  for f in /d/dev-*.db; do
    echo "#db $f"
    sqlite3 -readonly -separator "|" "$f" \
      "SELECT id,seq,opened_by,opened_at,COALESCE(opened_offset,-1),bytes FROM boots ORDER BY id DESC LIMIT 40;" 2>/dev/null
  done
'
