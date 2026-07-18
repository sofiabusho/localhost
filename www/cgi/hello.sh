#!/bin/bash
# Sample shell CGI — second interpreter alongside Python.
printf 'Content-Type: text/plain; charset=utf-8\n\n'
printf 'REQUEST_METHOD=%s\n' "${REQUEST_METHOD:-}"
printf 'PATH_INFO=%s\n' "${PATH_INFO:-}"
printf 'QUERY_STRING=%s\n' "${QUERY_STRING:-}"
printf 'CONTENT_LENGTH=%s\n' "${CONTENT_LENGTH:-}"
BODY=$(cat)
printf 'BODY=%s\n' "$BODY"
