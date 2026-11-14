WITH stats AS (SELECT COUNT(*) AS n, COALESCE(SUM(octet_length(payload)),0) AS bytes,
 COALESCE(MAX(octet_length(payload)),0)::bigint AS largest, MAX(id) AS h
 FROM root_filesystem_events WHERE path=$1),
 data AS (SELECT id AS seq,payload FROM root_filesystem_events
 WHERE path=$1 AND (SELECT n FROM stats)<=$2
 AND (SELECT bytes FROM stats)<=$3 AND (SELECT largest FROM stats)<=$4
 ORDER BY id ASC LIMIT $2)
 SELECT stats.n,stats.bytes,stats.largest,stats.h,data.seq,data.payload
 FROM stats LEFT JOIN data ON TRUE ORDER BY data.seq ASC
