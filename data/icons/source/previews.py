import sys,subprocess,io
from PIL import Image
D=sys.argv[1]
def r(svg,w,bg=None):
    cmd=["rsvg-convert","-w",str(w),f"{D}/{svg}"]
    return Image.open(io.BytesIO(subprocess.run(cmd,capture_output=True,check=True).stdout)).convert("RGBA")
def tile(im,bg,pad):
    t=Image.new("RGBA",(im.width+2*pad,im.height+2*pad),bg); t.alpha_composite(im,(pad,pad)); return t
def row(ims,bg="#f6f5f4",gap=24):
    W=sum(i.width for i in ims)+gap*(len(ims)+1); H=max(i.height for i in ims)+2*gap
    c=Image.new("RGBA",(W,H),bg); x=gap
    for i in ims: c.alpha_composite(i,(x,(H-i.height)//2)); x+=i.width+gap
    return c
A="io.github.yourtube.YourTube.svg"; S="io.github.yourtube.YourTube-symbolic.svg"
row([tile(r(A,128),"#ffffff",32),tile(r(A,128),"#242424",32),tile(r(A,64),"#ffffff",16),tile(r(A,32),"#ffffff",8)]).save(f"{D}/preview-128.png")
up=lambda im:im.resize((im.width*8,im.height*8),Image.NEAREST)
row([tile(r(A,16),"#ffffff",8),up(tile(r(A,16),"#ffffff",2)),tile(r(S,16),"#ffffff",8),up(tile(r(S,16),"#ffffff",2)),up(tile(r(S,16),"#d0d0d0",2))]).save(f"{D}/preview-16.png")
a=tile(r("yourtube-wordmark.svg",640),"#ffffff",40); b=tile(r("yourtube-wordmark-dark.svg",640),"#0d1117",40)
c=Image.new("RGBA",(a.width,a.height*2),"#fff"); c.alpha_composite(a,(0,0)); c.alpha_composite(b,(0,a.height)); c.save(f"{D}/preview-wordmark.png")
