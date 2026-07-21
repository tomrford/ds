interface MissingObjectRow extends Record<string, SqlStorageValue> {
  kind: number;
  id: ArrayBuffer;
}

export function findMissingReachableObject(
  sql: SqlStorage,
  rootKind: number,
  rootId: ArrayBuffer,
  implicitZeroKinds: readonly [number, ...number[]],
): MissingObjectRow | undefined {
  const zeroKinds = implicitZeroKinds.join(", ");
  return sql
    .exec<MissingObjectRow>(
      `WITH RECURSIVE reachable(kind, id) AS (
         VALUES (?, ?)
         UNION
         SELECT edges.referenced_kind, edges.referenced_id
         FROM reachable
         JOIN object_references AS edges
           ON edges.object_kind = reachable.kind
          AND edges.object_id = reachable.id
         LEFT JOIN complete_object_closures AS complete
           ON complete.kind = reachable.kind AND complete.id = reachable.id
         WHERE complete.id IS NULL
       )
       SELECT reachable.kind, reachable.id
       FROM reachable
       LEFT JOIN objects
         ON objects.kind = reachable.kind AND objects.id = reachable.id
       WHERE objects.id IS NULL
         AND NOT (reachable.kind IN (${zeroKinds}) AND reachable.id = zeroblob(64))
       ORDER BY reachable.kind, reachable.id
       LIMIT 1`,
      rootKind,
      rootId,
    )
    .toArray()[0];
}

export function markClosureComplete(sql: SqlStorage, rootKind: number, rootId: ArrayBuffer) {
  sql.exec(
    `INSERT OR IGNORE INTO complete_object_closures
     WITH RECURSIVE reachable(kind, id) AS (
       VALUES (?, ?)
       UNION
       SELECT edges.referenced_kind, edges.referenced_id
       FROM reachable
       JOIN object_references AS edges
         ON edges.object_kind = reachable.kind
        AND edges.object_id = reachable.id
       LEFT JOIN complete_object_closures AS complete
         ON complete.kind = reachable.kind AND complete.id = reachable.id
       WHERE complete.id IS NULL
     )
     SELECT kind, id FROM reachable`,
    rootKind,
    rootId,
  );
}
