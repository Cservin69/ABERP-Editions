import sys, math
sys.path.insert(0,"."); sys.path.insert(0,"aberp_cad_extract/tests")
sys.path.insert(0,"/private/tmp/claude-501/-Users-aben-Documents-Claude-Projects-ABERP-Editions/eef498db-c4ed-4805-adb7-98a4033940df/scratchpad")
import patch_buried3; patch_buried3.install(use_skin=True)
import aberp_cad_extract.holes as H
from test_holes import _boss_part, TOL
seed=[20260901]
def rnd(lo,hi):
    seed[0]=(1103515245*seed[0]+12345)%(2**31); return lo+(hi-lo)*(seed[0]/2**31)
wrong=0; considered=0
for _ in range(400):
    cz,cr,ch=rnd(3.,12.),rnd(7.,13.),rnd(13.,27.)
    bx,by,br=rnd(31.,39.5),rnd(18.,26.),rnd(2.1,4.9)
    off=math.hypot(40.-bx,20.-by)
    if off>=cr: continue
    want=cz+ch*(1.-off/cr)
    if want<=20.+TOL: continue
    considered+=1
    try:
        hs=[h for h in H.mine_cylindrical_holes(_boss_part(cz,cr,ch,bx,by,br))
            if abs(h.diameter_mm-2.*br)<TOL]
    except Exception:
        wrong+=1; continue
    if len(hs)!=1 or abs(hs[0].depth_mm-want)>1e-5: wrong+=1
print("PROTOTYPE C: considered=%d wrong=%d (baseline 10)"%(considered,wrong))
print("stats:",patch_buried3.STATS)
