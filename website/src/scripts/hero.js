/**
 * Hero section animations
 * Includes logo, tagline, subtitle, and scroll indicator
 */

export function initHeroAnimations() {
    const ironRing = document.getElementById('iron-ring');
    if (ironRing) {
        const isMobile = window.innerWidth < 768;
        gsap.to(ironRing, {
            rotation: 360,
            duration: 30,
            repeat: -1,
            ease: 'none',
            force3D: true
        });

        // Scale down on mobile
        if (isMobile) {
            gsap.set(ironRing, { scale: 0.7 });
        }
    }

    const logo = document.getElementById('logo');
    if (logo) {
        gsap.to(logo, {
            opacity: 1,
            scale: 1,
            duration: 0.8,
            ease: 'power4.out',
            delay: 0.2,
            force3D: true
        });
    }

    const tagline = document.getElementById('tagline');
    if (tagline) {
        gsap.to(tagline, {
            opacity: 1,
            y: 0,
            duration: 0.7,
            ease: 'power4.out',
            delay: 0.5,
            force3D: true
        });
    }

    const subtitle = document.getElementById('subtitle');
    if (subtitle) {
        gsap.to(subtitle, {
            opacity: 1,
            y: 0,
            duration: 0.7,
            ease: 'power4.out',
            delay: 0.7,
            force3D: true
        });
    }

    const scrollIndicator = document.getElementById('scroll-indicator');
    if (scrollIndicator) {
        gsap.to(scrollIndicator, {
            opacity: 1,
            duration: 0.5,
            delay: 1.2,
            force3D: true
        });
    }
}

