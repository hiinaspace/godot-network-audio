#!/usr/bin/env python3

import json
import os
import pathlib
import sys
import time


pid = int(sys.argv[1])
output = pathlib.Path(sys.argv[2])
clock_ticks = os.sysconf("SC_CLK_TCK")
page_size = os.sysconf("SC_PAGE_SIZE")

with output.open("w") as trace:
    while True:
        try:
            fields = pathlib.Path(f"/proc/{pid}/stat").read_text().split()
        except (FileNotFoundError, ProcessLookupError):
            break
        trace.write(
            json.dumps(
                {
                    "unix_usec": time.time_ns() // 1000,
                    "cpu_seconds": (int(fields[13]) + int(fields[14])) / clock_ticks,
                    "rss_kib": int(fields[23]) * page_size // 1024,
                },
                separators=(",", ":"),
            )
            + "\n"
        )
        trace.flush()
        time.sleep(0.05)
