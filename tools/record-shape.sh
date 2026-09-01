DB=/d/dev-_dev_serial_by-id_usb-Arduino_Bughopper_DK0HEVIC-e48f4f41.db
apk add -q sqlite 2>/dev/null
LO=$1; HI=$2
# Epoch rows, then the lines belonging to them. Tab-separated, one section each.
echo "#boots	id	seq	opened_by	opened_at	opened_offset	bytes	closed_at"
sqlite3 -readonly -separator '	' "$DB" \
  "SELECT 'boot', id, seq, opened_by, opened_at, COALESCE(opened_offset,-1), bytes, COALESCE(closed_at,-1)
     FROM boots WHERE id BETWEEN $LO AND $HI ORDER BY id;"
echo "#lines	id	boot_id	ts_wall	stream_offset	terminator	text"
sqlite3 -readonly -separator '	' "$DB" \
  "SELECT 'line', id, COALESCE(boot_id,-1), ts_wall, stream_offset, terminator,
          replace(replace(CAST(bytes AS TEXT), char(9), ' '), char(10), ' ')
     FROM raw_lines WHERE boot_id BETWEEN $LO AND $HI ORDER BY id;"
echo "#meta	key	value"
sqlite3 -readonly -separator '	' "$DB" \
  "SELECT 'meta', key, value FROM meta WHERE key LIKE 'pending%';"
