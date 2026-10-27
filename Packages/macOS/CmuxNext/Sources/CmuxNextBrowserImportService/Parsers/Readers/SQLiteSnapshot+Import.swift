import CmuxNextBrowserImport
import CmuxNextSQLiteSnapshot

extension SQLiteSnapshot {
    /// The pages a history query leaves out in SQL, before its `ORDER BY … LIMIT`: the address is not
    /// importable (``ImportableURL/sqlFilter(_:)``), or else the time is not `kept`. `source` is the
    /// query's `FROM … WHERE` naming every page it would read.
    func skippedPages(url: String, kept: String, source: String) throws -> ImportSkipCounts {
        let importable = ImportableURL.sqlFilter(url)
        var skipped = ImportSkipCounts()
        let sql = """
            SELECT SUM(CASE WHEN \(importable) THEN 0 ELSE 1 END),
                   SUM(CASE WHEN \(importable) THEN (CASE WHEN \(kept) THEN 0 ELSE 1 END) ELSE 0 END) \(source)
            """
        try query(sql) { row in
            skipped.notImportable = row.int(0)
            skipped.badTime = row.int(1)
            return false
        }
        return skipped
    }
}
