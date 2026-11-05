#!/usr/bin/env python3
"""Reproduce the pinned Qoder resources and official JS/WASM test vectors.

Downloads and verifies the npm archive; never starts the CLI or native SDK.
--fixtures executes only the isolated wasm-bindgen module with fake credentials,
fixed time and deterministic test randomness. Requires Node >= 20 for that flag.
"""
import argparse
import base64
import gzip
import hashlib
import io
import json
import pathlib
import re
import subprocess
import tarfile
import tempfile
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parents[1]
CRATE = ROOT / "src-tauri/crates/qoder-client"
URL = "https://registry.npmjs.org/@qoder-ai/qodercli/-/qodercli-1.1.83.tgz"
INTEGRITY = "sha512-NX2JgHw5IUctFX8pTbcQnpA7pEYfn1ZLWv9HfatVyAN3X2x5LZfqP+t4zFRik+VZ9f5/7bFesiP6qT62kXTW+g=="
BINARIES = {"gPc": "darwin-arm64", "fPc": "darwin-x64", "pPc": "linux-x64", "mPc": "linux-arm64", "EPc": "win32-x64", "hPc": "sgsdk-win32-x64"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixtures", action="store_true")
    args = parser.parse_args()
    with urllib.request.urlopen(URL, timeout=60) as response:
        archive = response.read(40 * 1024 * 1024)
    actual = "sha512-" + base64.b64encode(hashlib.sha512(archive).digest()).decode()
    if actual != INTEGRITY:
        raise SystemExit("npm integrity mismatch")
    with tarfile.open(fileobj=io.BytesIO(archive), mode="r:gz") as package:
        source = package.extractfile("package/bundle/qodercli.js").read().decode()
        license_text = package.extractfile("package/LICENSE").read()
    vendor = CRATE / "vendor"
    vendor.mkdir(parents=True, exist_ok=True)
    (vendor / "LICENSE-APACHE-2.0").write_bytes(license_text)
    wasm = base64.b64decode(re.search(r'Owc="(AGFzbQ[A-Za-z0-9+/=]+)"', source)[1])
    digest = hashlib.sha256(wasm).hexdigest()
    if digest != "c7a14b11d6c23b94ac39f6868ebd3dd6a70492efe87c864c33857adadb9f0951":
        raise SystemExit("Unexpected WASM module")
    (vendor / "qoder_auth.wasm").write_bytes(wasm)
    manifest = {"package": "@qoder-ai/qodercli", "version": "1.1.83", "tarball": URL, "integrity": INTEGRITY, "files": {"qoder_auth.wasm": {"sha256": digest}}}
    for variable, platform in BINARIES.items():
        binary = base64.b64decode(re.search(r'\b' + variable + r'="([A-Za-z0-9+/=]+)"', source)[1])
        name = f"runtime-info-{platform}.gz"
        # GzipFile fixes the OS byte and compression level across Python versions.
        output = io.BytesIO()
        with gzip.GzipFile(fileobj=output, mode="wb", filename="", compresslevel=9, mtime=0) as stream:
            stream.write(binary)
        compressed = output.getvalue()
        (vendor / name).write_bytes(compressed)
        manifest["files"][name] = {"sha256": hashlib.sha256(compressed).hexdigest(), "uncompressed_sha256": hashlib.sha256(binary).hexdigest(), "uncompressed_size": len(binary)}
    (vendor / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    if args.fixtures:
        start = source.index("Ipi={}")
        end = source.index("});import*as t3A", start) + 3
        prefix = r'''import fs from 'node:fs';
const _$d1=(s,k="27Kmd7lNo5CM")=>{const b=Buffer.from(s,"base64");for(let i=0;i<b.length;i++)b[i]^=k.charCodeAt(i%k.length);return b.toString()};
const C=(A,e)=>()=>(A&&(e=A(A=0)),e),YA=(A,e)=>{for(const t in e)Object.defineProperty(A,t,{get:e[t],enumerable:true})};
let counter=0;Object.defineProperty(globalThis,'crypto',{value:{getRandomValues(a){for(let i=0;i<a.length;i++)a[i]=(++counter%255)+1;return a;}}});Date.now=()=>1800000000000;
var '''
        trailer = r'''
Bpi();i0c({module:fs.readFileSync(process.argv[2])});
const identity={uid:'fixture-user',security_oauth_token:'fixture-token',organization_id:'',data_policy_agreed:true};
const fields=JSON.parse(Vwc(JSON.stringify(identity)));
const context=new BlA('fixture-machine','1.1.83',JSON.stringify({...identity,...fields}),JSON.stringify({client_type:'5',business_product:'cli',business_type:'agent',scene:'assistant'}));
const body=JSON.stringify({messages:[{role:'user',content:'fixture 你好'}],model_config:{key:'fixture-model'}});
function unpack(r){try{return{url:r.url,body:r.body,headers:Object.fromEntries(r.headers)}}finally{r.free()}}
const inference=unpack(context.prepareInferRequest('https://api3.qoder.sh',body,'fixture-model','system'));
const catalog=unpack(context.prepareRequest('https://api3.qoder.sh','/api/v2/model/list','GET','auth','{}',undefined));
if(jwc(inference.body)!==body)throw Error('Official response decoder mismatch');
console.log(JSON.stringify({identity,body,inference,catalog}));
'''
        with tempfile.TemporaryDirectory(prefix="qoder-wasm-reference-") as directory:
            program = pathlib.Path(directory) / "reference.mjs"
            program.write_text(prefix + source[start:end] + trailer)
            data = subprocess.check_output(["node", str(program), str(vendor / "qoder_auth.wasm")], timeout=30)
        (CRATE / "tests/fixtures/official-1.1.83.json").write_text(json.dumps(json.loads(data), ensure_ascii=False, indent=2) + "\n")
    print("Verified Qoder CLI 1.1.83 resources" + (" and reproduced official WASM fixtures" if args.fixtures else ""))


if __name__ == "__main__":
    main()
