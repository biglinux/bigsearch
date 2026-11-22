# big-docinfo

Reads a document and suggests a file name from what it says: its kind, date,
entity and title. The Dolphin action "Suggest a name…" (`packaging/suggest-name`)
shows the suggestion and renames the file only when the person confirms.

```sh
big-docinfo FILE                # what it found, one field per line, then the name
big-docinfo --name FILE         # only the suggested name, free in that folder
big-docinfo --json FILE         # kind, date, entity, title, highlights,
                                # suggested_name, free_name, kept_name
big-docinfo rename FILE NAME    # rename, keeping the extension; never replaces a file
```

`free_name` is the suggestion with ` (2)`, ` (3)`… added when the folder
already holds that name; `kept_name` is true when the document says too little
and the current name stays. `rename` refuses a name that holds a path or starts
with a dot.

| Exit status | Meaning |
|---|---|
| 0 | done |
| 1 | the file has no readable text, the rename was refused, or another error; the message says which |
| 2 | the arguments name no command |
