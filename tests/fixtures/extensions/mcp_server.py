#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Small real MCP server for stdio/HTTP discovery and model invocation checks."""
import argparse
import json
import secrets
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument('--http', type=int)
parser.add_argument('--audit-file')
args = parser.parse_args()
proof = 'mcp-' + secrets.token_hex(12)


def response(message):
    method = message.get('method')
    if 'id' not in message:
        return None
    if method == 'initialize':
        result = {'protocolVersion': message['params']['protocolVersion'], 'capabilities': {'tools': {}}, 'serverInfo': {'name': 'extension-check', 'version': '1.0.0'}}
    elif method == 'tools/list':
        result = {'tools': [{'name': 'fetch_device_proof', 'description': 'Return a fresh verification proof from this MCP process.', 'inputSchema': {'type': 'object', 'properties': {}, 'additionalProperties': False}, 'annotations': {'readOnlyHint': True, 'destructiveHint': False}}]}
    elif method == 'tools/call' and message.get('params', {}).get('name') == 'fetch_device_proof':
        if args.audit_file:
            with Path(args.audit_file).open('a') as output:
                output.write(json.dumps({'method': method, 'proof': proof}) + '\n')
        result = {'content': [{'type': 'text', 'text': proof}], 'isError': False}
    elif method == 'ping':
        result = {}
    else:
        return {'jsonrpc': '2.0', 'id': message['id'], 'error': {'code': -32601, 'message': 'Unknown method'}}
    return {'jsonrpc': '2.0', 'id': message['id'], 'result': result}


if args.http is None:
    for line in sys.stdin:
        result = response(json.loads(line))
        if result is not None:
            print(json.dumps(result), flush=True)
else:
    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            result = response(json.loads(self.rfile.read(int(self.headers['Content-Length']))))
            body = json.dumps(result).encode() if result is not None else b''
            self.send_response(200 if result is not None else 202)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            self.send_error(405)

        def do_DELETE(self):
            self.send_response(200)
            self.end_headers()

        def log_message(self, *_):
            pass

    HTTPServer(('127.0.0.1', args.http), Handler).serve_forever()
