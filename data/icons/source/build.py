import sys, os
from geom import *
from fontTools.ttLib import TTFont
from fontTools.varLib.instancer import instantiateVariableFont
from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.pens.transformPen import TransformPen
OUT=sys.argv[1]; os.makedirs(OUT,exist_ok=True)
CORAL_T,CORAL_B,DEPTH,CHIM,CREAM,INK="#FF6B4F","#EC4A36","#B5352B","#D63F2F","#FFF7EE","#241F31"

def house(s,ox=0,oy=0):
    # 256-unit master geometry scaled by s
    P=lambda pts:[(ox+x*s,oy+y*s) for x,y in pts]
    body=rounded_poly(P([(16,92),(128,40),(240,92),(240,212),(16,212)]),[x*s for x in (16,18,16,40,40)])
    chim=rounded_poly(P([(180,34),(206,34),(206,100),(180,100)]),[x*s for x in (6,6,0.01,0.01)])
    play=tri(ox+122*s,oy+152*s,84*s,10*s)
    return body,chim,play

# ---- full colour app icon, 128 ----
s=0.5; body,chim,play=house(s,0,4)
body_d,_,_=house(s,0,10)  # depth slab, shifted down 6px
icon=f'''<svg xmlns="http://www.w3.org/2000/svg" width="128" height="128" viewBox="0 0 128 128">
  <title>YourTube</title>
  <defs>
    <linearGradient id="face" x1="0" y1="20" x2="0" y2="110" gradientUnits="userSpaceOnUse">
      <stop offset="0" stop-color="{CORAL_T}"/><stop offset="1" stop-color="{CORAL_B}"/>
    </linearGradient>
  </defs>
  <path fill="{CHIM}" d="{chim}"/>
  <path fill="{DEPTH}" d="{body_d}"/>
  <path fill="url(#face)" d="{body}"/>
  <path fill="{CREAM}" d="{play}"/>
</svg>
'''
open(f"{OUT}/io.github.yourtube.YourTube.svg","w").write(icon)

# ---- symbolic 16 (hand-tuned small cut) ----
P=[(1,6),(8,2),(15,6),(15,14),(1,14)]
sb=rounded_poly(P,[0.8,0.9,0.8,2,2])
sc=rounded_poly([(11,1),(13,1),(13,5),(11,5)],[0.5,0.5,0.01,0.01])
sp=rounded_poly([(6,7),(11,10),(6,13)],0.5)
sym=f'''<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16">
  <path fill="#2e3436" d="{sc}"/>
  <path fill="#2e3436" fill-rule="evenodd" d="{sb} {sp}"/>
</svg>
'''
open(f"{OUT}/io.github.yourtube.YourTube-symbolic.svg","w").write(sym)

# ---- wordmark ----
f=TTFont("/usr/share/fonts/Adwaita/AdwaitaSans-Regular.ttf")
f=instantiateVariableFont(f,{"wght":700})
gs=f.getGlyphSet(); cmap=f.getBestCmap(); upm=f["head"].unitsPerEm
hmtx=f["hmtx"]
def text_path(txt,x0,base,size,track=0):
    sc=size/upm; x=x0; d=""
    for ch in txt:
        g=cmap[ord(ch)]; pen=SVGPathPen(gs)
        gs[g].draw(TransformPen(pen,(sc,0,0,-sc,x,base)))
        d+=pen.getCommands()+" "; x+=hmtx[g][0]*sc+track
    return d,x
H=96  # lockup height
ms=H/256*1.0
b2,c2,p2=house(ms*1.0,0,-4*ms)
size=64; base=74; track=-1.2
d1,x1=text_path("Your",112,base,size,track)
d2,x2=text_path("Tube",x1,base,size,track)
W=round(x2+4)
def wm(tube_col,tri_col):
    return f'''<svg xmlns="http://www.w3.org/2000/svg" width="{W*2}" height="{H*2}" viewBox="0 0 {W} {H}">
  <title>YourTube</title>
  <defs>
    <linearGradient id="face" x1="0" y1="0" x2="0" y2="{H}" gradientUnits="userSpaceOnUse">
      <stop offset="0" stop-color="{CORAL_T}"/><stop offset="1" stop-color="{CORAL_B}"/>
    </linearGradient>
  </defs>
  <path fill="{CHIM}" d="{c2}"/>
  <path fill="url(#face)" d="{b2}"/>
  <path fill="{tri_col}" d="{p2}"/>
  <path fill="{CORAL_B}" d="{d1}"/>
  <path fill="{tube_col}" d="{d2}"/>
</svg>
'''
open(f"{OUT}/yourtube-wordmark.svg","w").write(wm(INK,CREAM))
open(f"{OUT}/yourtube-wordmark-dark.svg","w").write(wm("#FFFFFF",CREAM))
print("W",W)
