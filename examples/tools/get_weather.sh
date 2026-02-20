#!/bin/bash
# Example tool: returns mock weather data for a location.
# Arguments are passed as JSON on stdin.
#
# Usage with onwards:
#   Configure tools_dir in your config file:
#     { "tools_dir": "./examples/tools", ... }
#
#   The LLM can then call "get_weather" with {"location": "London"}.

INPUT=$(cat)
LOCATION=$(echo "$INPUT" | grep -o '"location":"[^"]*"' | head -1 | cut -d'"' -f4)
: "${LOCATION:=unknown}"

echo "{\"temperature\": 72, \"conditions\": \"sunny\", \"location\": \"$LOCATION\"}"
