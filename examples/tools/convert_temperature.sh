#!/bin/bash
# Converts temperature between Fahrenheit and Celsius.
INPUT=$(cat)
VALUE=$(echo "$INPUT" | grep -o '"value":[0-9.-]*' | head -1 | cut -d':' -f2)
FROM=$(echo "$INPUT" | grep -o '"from":"[^"]*"' | head -1 | cut -d'"' -f4)

if [ -z "$VALUE" ] || [ -z "$FROM" ]; then
  echo '{"error": "missing value or from field"}'
  exit 1
fi

case "$(echo "$FROM" | tr '[:upper:]' '[:lower:]')" in
  f|fahrenheit)
    RESULT=$(echo "scale=1; ($VALUE - 32) * 5 / 9" | bc -l)
    echo "{\"original\": $VALUE, \"original_unit\": \"F\", \"converted\": $RESULT, \"converted_unit\": \"C\"}" ;;
  c|celsius)
    RESULT=$(echo "scale=1; $VALUE * 9 / 5 + 32" | bc -l)
    echo "{\"original\": $VALUE, \"original_unit\": \"C\", \"converted\": $RESULT, \"converted_unit\": \"F\"}" ;;
  *)
    echo "{\"error\": \"unknown unit: $FROM, use F or C\"}" ;;
esac
