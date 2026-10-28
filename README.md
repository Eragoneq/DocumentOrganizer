# Document Organizer

Rust desktop application for reviewing and organizing local documents with user-managed AI endpoints for privacy responsible users.

## Technologies

- GPUI - for native UI  
- SQLite - for storing the catalog, extracted content and revisions, tags, jobs, approvals, and move journal

## Build and run

Requires Windows x64, Rustup, Visual Studio C++ Build Tools, and a Windows SDK. `rust-toolchain.toml` selects Rust 1.95.0 for this project. Direct dependencies and `Cargo.lock` are pinned.

From this directory, in PowerShell:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\setup-pdfium.ps1
cargo run --locked
```

The setup script downloads PDFium **7543**, verifies its SHA256, and installs its DLL and notices beside the debug executable. This version matches `pdfium-render` 0.8.37. Alternatively, select a compatible DLL in Settings. Missing PDFium is recorded as an extraction error; the file remains in the catalog.

To build a portable release folder:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\package.ps1
```

The OS application-data directory is used by default. Set `DOCUMENT_ORGANIZER_DATA` to an absolute directory to choose another location. It contains `catalog.sqlite3` and a disposable `previews` directory. SQLite uses foreign keys, WAL, and full synchronous journaling. Back up while the app is closed, or through SQLite's backup API; an active WAL is part of the database state.

## Models

Set complete URLs in **Settings**:

- Decision: `http://127.0.0.1:8080/v1/systemone`, with d1 text and image support.
- Generative: `http://127.0.0.1:8081/v1/chat/completions`, plus your generative model ID. This endpoint only proposes tags.
- Decision model version: an audit label for the model revision and quantization you serve; the endpoint does not reliably report its weight revision.

Use both connection-test buttons. The app does not install or launch models. Named `choice` questions produce relevance and a filing category; independent `noul` probabilities score tags, matching [d1's documented interface](https://huggingface.co/LiquidAI/d1-3B-GGUF). Requests time out after 60 seconds. Cancellation discards responses received afterward; an in-flight request can take up to that timeout to finish.

## Review and organization

1. Add directories in **Sources**, then **Scan**. Overlapping roots are deduplicated. Links and junctions are not followed. Skipped paths and reasons appear under Sources; Include overrides take effect on the next scan.
2. Review the virtualized list. Search covers filenames, original paths, and cached extracted text via SQLite FTS5. Use the relevance/tag filters, text/image/PDF previews, categories, and manual tag choices. Thumbnails are cached, PDF pages render lazily, and the next item's preview is prefetched.
3. Add tags and descriptions, or request suggestions. Suggestions are inert until accepted. Adding, accepting, or editing a tag queues scoring of previously relevant items. Manual choices survive rescoring. Tag scores do not change folders or destination approvals.
4. **Prepare moves**, review the exact destinations under **Moves**, and approve individually or as a selected group. **Move approved** executes them. Changes to items, categories, or destinations invalidate approval. Filenames are preserved; existing destinations are blocked.

Projects form one outermost classification/movement unit. Their generated files and dependencies stay inside the folder even when excluded from analysis. Partial-project selections and linked Git worktrees cannot move. Project membership takes precedence over file type.

Actual OS Pictures, Documents, Music, and Videos folders are resolved with `UserDirs`; all destinations can be overridden. Projects use Documents/Projects/category; other files use Documents/Other/category. Missing standard folders require an explicit destination.

Shortcuts: Ctrl+Down / Ctrl+Up for next / previous; Ctrl+Enter for approve and next; Ctrl+Alt+Z for the latest eligible undo. Background work stays off the UI thread. Endpoint failures pause analysis; **Retry** resumes failed/cancelled jobs. Interrupted running jobs return to the queue after restart.

<!--## Filesystem operations

Same-volume moves use Windows `MoveFileExW` without replacement after source verification. Windows requires descendant file handles to close before a directory rename. Cross-volume moves copy to a unique temporary destination, flush and verify it, publish it, then remove verified source entries; files remain locked against concurrent writers throughout that workflow. Modification times, read-only attributes, and Windows alternate data streams are preserved by the copy path. Whole project folders move, and descendant current paths update together. Cross-volume projects containing symbolic links, junctions, or hard links are blocked. ACL inheritance is not replicated.

SQLite records move stages and temporary paths. Startup reconciles interrupted operations. If both copies remain, explicit **Retry recovery** is required. Unverifiable destinations never authorize source removal. Failed temporary copies remain for inspection; retry uses a fresh temporary name. Completed moves can be undone if content is unchanged and the original location is free. Original paths stay immutable. No directory merging, automatic deduplication, or deletion of excluded files is performed.-->

## Content limits

Digital PDFs and recognized UTF-8 text/code files are extracted, including common extensionless source files. Extraction is bounded to 1 MiB, and PDFs to 100 pages. Long model inputs use head/middle/tail excerpts; limitations appear in details. Images are resized to at most 1024 pixels for classification, leaving originals intact. Locked files are cataloged for review and retried on a manual rescan. Scanned PDFs, extraction failures, and unsupported formats remain reviewable. Office and other unsupported content use metadata plus **Open original**. OCR, Office conversion, watching, embeddings, cloud integrations, and dashboards are deferred.

# Work in Progress

This is still a project mainly intended for personal use, as such it may contain bugs and not work on certain evnironments.
