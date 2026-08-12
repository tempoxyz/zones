# tempo-xtask

A polyfill to perform various operations on the codebase.

Subcommands currently supported:

+ `create-zone`: creates a new zone through Tempo's native TIP-1091 ZoneFactory.
+ `generate-zone-genesis`: generates a zone L2 genesis file.
+ `pause-portal`: pauses new deposits and L1 withdrawal processing.
+ `resume-portal`: resumes new deposits and L1 withdrawal processing (admin only).
