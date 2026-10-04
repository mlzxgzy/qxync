import json, glob, os, re

root = '/home/kami/项目/qxync/xtask/probe/probe-out'
files = sorted(glob.glob(root + '/**/*.http', recursive=True))
rows = []
for f in files:
    t = open(f, encoding='utf-8', errors='replace').read()
    if 'func=get_list' not in t:
        continue
    head = t.splitlines()[0] if t.splitlines() else ''
    m = re.search(r'limit=(\d+)', head)
    s = re.search(r'start=(\d+)', head)
    i = t.find('{')
    try:
        d = json.loads(t[i:])
        total, real, n = d.get('total'), d.get('real_total'), len(d.get('datas', []))
    except Exception:
        total, real, n = 'PARSE_ERR', None, None
    rows.append((os.path.relpath(f, root), m.group(1) if m else '?',
                 s.group(1) if s else '?', total, real, n))

print(f"{'file':<44} {'limit':>5} {'start':>5} {'total':>6} {'real_tot':>8} {'datas':>6}")
for r in rows:
    print(f"{r[0]:<44} {str(r[1]):>5} {str(r[2]):>5} {str(r[3]):>6} {str(r[4]):>8} {str(r[5]):>6}")
