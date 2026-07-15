#!/bin/bash
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
"$SCRIPT_DIR/target/debug/fan" -i 2 -c 0:0,40:40,45:100,48:150,50:200,55:230,60:255 -n 200 -p 60006
