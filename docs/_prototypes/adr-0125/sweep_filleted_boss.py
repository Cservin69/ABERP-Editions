import sys, math
sys.path.insert(0,"."); sys.path.insert(0,"aberp_cad_extract/tests")
sys.path.insert(0,"/private/tmp/claude-501/-Users-aben-Documents-Claude-Projects-ABERP-Editions/eef498db-c4ed-4805-adb7-98a4033940df/scratchpad")
MODE=sys.argv[1]
if MODE=="m5":
    import patch_buried3; patch_buried3.install(use_skin=True)
import aberp_cad_extract.holes as H
from adv_parts import filleted_boss
TOL=1e-6
print("mode=%s"%MODE)
rows=[]
for cone_r,cone_h,fil,off in [(10.0,18.0,2.0,1.0),(10.0,18.0,3.0,1.5),
                              (12.0,22.0,2.5,0.8),(9.0,16.0,1.5,2.0),
                              (11.0,25.0,4.0,1.2),(8.0,14.0,2.0,0.5)]:
    bx,by,br = 20.0+off, 20.0, 4.0
    want = 20.0 + cone_h*(1.0-off/cone_r)      # P: cone surface on the bore axis
    try:
        hs=[h for h in H.mine_cylindrical_holes(
                filleted_boss(20.0,20.0,cone_r,cone_h,fil,bx,by,br))
            if abs(h.diameter_mm-2*br)<1e-6]
    except Exception as e:
        rows.append((cone_r,cone_h,fil,off,want,"RAISED:"+type(e).__name__)); continue
    got = hs[0].depth_mm if len(hs)==1 else "n=%d"%len(hs)
    rows.append((cone_r,cone_h,fil,off,want,got))
for cr,ch,fil,off,want,got in rows:
    ok = isinstance(got,float) and abs(got-want)<=1e-5
    print("  cr=%-5.1f ch=%-5.1f fillet=%-4.1f off=%-4.1f want=%-9.4f got=%-12s %s"
          %(cr,ch,fil,off,want,(round(got,4) if isinstance(got,float) else got),
            "ok" if ok else "<<< DIFFERS"))
