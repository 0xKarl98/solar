import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import time

root = Path(__file__).resolve().parent
branch = 'perf/lsp-member-position-index'
bench_path = Path('crates/lsp/benches/lsp.rs')
def git(*args):
    return subprocess.check_output(['git', *args], text=True).strip()
assert git('branch', '--show-current') == branch
assert not git('status', '--porcelain')
base = git('rev-parse', 'origin/main')
head = git('rev-parse', 'HEAD')
assert git('rev-parse', 'HEAD^') == base
fixture = bench_path.read_bytes()
metadata = {'baseline_commit': base, 'candidate_commit': head, 'platform': platform.platform(), 'rustc': subprocess.check_output(['rustc','-Vv'],text=True), 'fixture_sha256': hashlib.sha256(fixture).hexdigest(), 'runs': []}
pattern = r'^lsp/(completion/|member-completion/|open-document-selection-range-line-layout/|open-document-selection-range/(uniswap-v3|unifap-v2-router)$|project-analysis(-after-edit)?/unifap-v2$)'
def measure(name, binary, source_commit, pattern):
    folder = root / name
    folder.mkdir()
    command = [str(binary), '--bench', '--noplot', '--sample-size', '40', '--warm-up-time', '1', '--measurement-time', '2', pattern]
    env = os.environ.copy()
    env['CRITERION_HOME'] = str(folder / 'criterion')
    start = time.time()
    print('Measuring '+name, flush=True)
    with (folder / 'run.log').open('w') as log:
        status = subprocess.run(command, env=env, stdout=log, stderr=subprocess.STDOUT).returncode
    run = {'name':name,'source_commit':source_commit,'fixture_sha256':metadata['fixture_sha256'],'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),'command':command,'criterion_home':env['CRITERION_HOME'],'exit_code':status,'elapsed_seconds':time.time()-start}
    metadata['runs'].append(run)
    (root / 'run-metadata.json').write_text(json.dumps(metadata,indent=2)+'\n')
    if status:
        raise RuntimeError(f'{name}: {status}')
try:
    for role, revision in [('baseline',base),('candidate',head)]:
        subprocess.run(['git','switch','--detach',revision],check=True)
        bench_path.write_bytes(fixture)
        (root / f'{role}-fixture.diff').write_bytes(subprocess.check_output(['git','diff']))
        command = ['cargo','bench','--locked','-p','solar-lsp','--features','bench','--bench','lsp','--no-run']
        print('Building '+role+' at '+revision,flush=True)
        with (root / f'{role}-build.log').open('w') as log:
            subprocess.run(command,stdout=log,stderr=subprocess.STDOUT,check=True)
        build = (root / f'{role}-build.log').read_text()
        executable = Path(re.search(r'Executable benches/lsp.rs \(([^\n]+)\)',build).group(1))
        binary = root / (role+'-lsp')
        shutil.copy2(executable,binary)
        measure(role,binary,revision,pattern)
        subprocess.run(['git','restore','--',str(bench_path)],check=True)
    for role, revision in [('candidate',head),('baseline',base)]:
        measure(role+'-repeat',root/(role+'-lsp'),revision,r'^lsp/completion/')
finally:
    subprocess.run(['git','restore','--',str(bench_path)],check=True)
    subprocess.run(['git','switch',branch],check=True)
