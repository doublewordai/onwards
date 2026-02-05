#!/bin/bash
# Evaluates a math expression and returns the result.
INPUT=$(cat)
EXPR=$(echo "$INPUT" | grep -o '"expression":"[^"]*"' | head -1 | cut -d'"' -f4)

if [ -z "$EXPR" ]; then
  echo '{"error": "missing expression field"}'
  exit 1
fi

# Use bc for floating point math
RESULT=$(echo "$EXPR" | bc -l 2>&1)

if [ $? -ne 0 ]; then
  echo "{\"error\": \"invalid expression\", \"expression\": \"$EXPR\"}"
  exit 1
fi

# Trim trailing zeros
RESULT=$(echo "$RESULT" | sed 's/\.\{0,1\}0*$//')

echo "{\"expression\": \"$EXPR\", \"result\": $RESULT}"
