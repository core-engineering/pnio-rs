import struct, sys, datetime
from collections import Counter
def blocks(path):
    with open(path,'rb') as f: data=f.read()
    magic=struct.unpack_from('<I',data,0)[0]
    if magic in (0xa1b2c3d4,0xa1b23c4d,0xd4c3b2a1,0x4d3cb2a1):
        endian='<' if magic in (0xa1b2c3d4,0xa1b23c4d) else '>'
        nano=magic in (0xa1b23c4d,0x4d3cb2a1)
        off=24
        while off+16<=len(data):
            sec,frac,cap,orig=struct.unpack_from(endian+'IIII',data,off)
            yield sec+frac*(1e-9 if nano else 1e-6), data[off+16:off+16+cap]
            off+=16+cap
        return
    off=0; endian='<'; tsres=1e-6
    while off+8<=len(data):
        btype,blen=struct.unpack_from(endian+'II',data,off)
        if btype==0x0A0D0D0A:
            bom=struct.unpack_from('<I',data,off+8)[0]; endian='<' if bom==0x1A2B3C4D else '>'
            btype,blen=struct.unpack_from(endian+'II',data,off)
        elif btype==6:
            iface,th,tl,cap,orig=struct.unpack_from(endian+'IIIII',data,off+8)
            ts=((th<<32)|tl)*tsres
            yield ts, data[off+28:off+28+cap]
        off+=blen
def hms(t): return datetime.datetime.fromtimestamp(t, datetime.UTC).strftime('%H:%M:%S.%f')[:-3]
path=sys.argv[1]; thr=float(sys.argv[2]) if len(sys.argv)>2 else 0.0015
last={}; n=Counter(); big=[]
for ts,pkt in blocks(path):
    if len(pkt)<22: continue
    if pkt[12:14]==b'\x81\x00' and pkt[16:18]==b'\x88\x92': fid=int.from_bytes(pkt[18:20],'big')
    elif pkt[12:14]==b'\x88\x92': fid=int.from_bytes(pkt[14:16],'big')
    else: continue
    n[fid]+=1
    if fid in last and ts-last[fid]>thr: big.append((last[fid],ts-last[fid],fid))
    last[fid]=ts
print({hex(k):v for k,v in n.items()})
for t,d,fid in sorted(big):
    if fid in (0x8000,0x8001): print(f"{hms(t)} {hex(fid)} gap {d*1e3:8.3f} ms")
