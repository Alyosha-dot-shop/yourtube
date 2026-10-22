import math
def rounded_poly(pts, radii):
    """Closed polygon with circular fillets; radii per vertex. Returns SVG path d."""
    n=len(pts); segs=[]
    for i in range(n):
        p0=pts[i-1]; p=pts[i]; p1=pts[(i+1)%n]; r=radii[i] if isinstance(radii,(list,tuple)) else radii
        v0=(p0[0]-p[0],p0[1]-p[1]); v1=(p1[0]-p[0],p1[1]-p[1])
        l0=math.hypot(*v0); l1=math.hypot(*v1)
        u0=(v0[0]/l0,v0[1]/l0); u1=(v1[0]/l1,v1[1]/l1)
        ang=math.acos(max(-1,min(1,u0[0]*u1[0]+u0[1]*u1[1])))
        t=r/math.tan(ang/2)
        a=(p[0]+u0[0]*t,p[1]+u0[1]*t); b=(p[0]+u1[0]*t,p[1]+u1[1]*t)
        cross=u0[0]*u1[1]-u0[1]*u1[0]
        sweep=1 if cross<0 else 0
        segs.append((a,b,r,sweep))
    d=f"M{segs[0][1][0]:.2f} {segs[0][1][1]:.2f}"
    for i in list(range(1,n))+[0]:
        a,b,r,s=segs[i]
        d+=f" L{a[0]:.2f} {a[1]:.2f} A{r:.2f} {r:.2f} 0 0 {s} {b[0]:.2f} {b[1]:.2f}"
    return d+" Z"
def tri(cx,cy,h,r):
    # right-pointing equilateral-ish triangle, centroid at cx,cy, height h (vertical side length)
    w=h*math.sqrt(3)/2
    pts=[(cx-w/3,cy-h/2),(cx+2*w/3,cy),(cx-w/3,cy+h/2)]
    return rounded_poly(pts,r)
