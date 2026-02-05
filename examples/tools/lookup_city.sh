#!/bin/bash
# Returns mock city metadata. The model needs this to feed into other tools.
INPUT=$(cat)
CITY=$(echo "$INPUT" | grep -o '"city":"[^"]*"' | head -1 | cut -d'"' -f4)

case "$(echo "$CITY" | tr '[:upper:]' '[:lower:]')" in
  london)
    echo '{"city": "London", "country": "United Kingdom", "timezone": "Europe/London", "population": 8982000, "currency": "GBP"}' ;;
  tokyo)
    echo '{"city": "Tokyo", "country": "Japan", "timezone": "Asia/Tokyo", "population": 13960000, "currency": "JPY"}' ;;
  new\ york|"new york")
    echo '{"city": "New York", "country": "United States", "timezone": "America/New_York", "population": 8336000, "currency": "USD"}' ;;
  paris)
    echo '{"city": "Paris", "country": "France", "timezone": "Europe/Paris", "population": 2161000, "currency": "EUR"}' ;;
  *)
    echo "{\"city\": \"$CITY\", \"country\": \"Unknown\", \"timezone\": \"UTC\", \"population\": 0, \"currency\": \"Unknown\"}" ;;
esac
