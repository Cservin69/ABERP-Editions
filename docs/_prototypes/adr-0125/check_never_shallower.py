import sys, math
sys.path.insert(0,"."); sys.path.insert(0,"aberp_cad_extract/tests")
sys.path.insert(0,"/private/tmp/claude-501/-Users-aben-Documents-Claude-Projects-ABERP-Editions/eef498db-c4ed-4805-adb7-98a4033940df/scratchpad")
import importlib
def sweep(with_m5):
    for m in list(sys.modules):
        if m.startswith("aberp_cad_extract") or m=="test_holes": del sys.modules[m]
    import aberp_cad_extract.holes as H
    if with_m5:
        import patch_buried3; importlib.reload(patch_buried3); patch_buried3.install(True)
    from test_holes import _boss_part, TOL
    seed=[20260901]
    def rnd(lo,hi):
        seed[0]=(1103515245*seed[0]+12345)%(2**31); return lo+(hi-lo)*(seed[0]/2**31)
    out=[]
    for _ in range(400):
        cz,cr,ch=rnd(3.,12.),rnd(7.,13.),rnd(13.,27.)
        bx,by,br=rnd(31.,39.5),rnd(18.,26.),rnd(2.1,4.9)
        off=math.hypot(40.-bx,20.-by)
        if off>=cr: continue
        want=cz+ch*(1.-off/cr)
        if want<=20.+TOL: continue
        try:
            hs=[h for h in H.mine_cylindrical_holes(_boss_part(cz,cr,ch,bx,by,br))
                if abs(h.diameter_mm-2.*br)<TOL]
            out.append(hs[0].depth_mm if len(hs)==1 else None)
        except Exception: out.append(None)
    return out
a=sweep(False); b=sweep(True)
pairs=[(x,y) for x,y in zip(a,b) if x is not None and y is not None]
deeper=sum(1 for x,y in pairs if y>x+1e-9)
same=sum(1 for x,y in pairs if abs(y-x)<=1e-9)
shallow=[(x,y) for x,y in pairs if y<x-1e-9]
print("compared=%d  deeper=%d  unchanged=%d  SHALLOWER=%d"%(len(pairs),deeper,same,len(shallow)))
for x,y in shallow[:5]: print("   %.4f -> %.4f"%(x,y))
