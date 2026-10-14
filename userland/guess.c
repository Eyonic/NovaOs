/* An interactive C program: reads the terminal through stdin. */
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

int main(void) {
    srand((unsigned)time(NULL));
    int secret = rand() % 100 + 1, guess = 0, tries = 0;
    printf("I'm thinking of a number between 1 and 100.\n");
    while (guess != secret) {
        printf("Your guess: ");
        fflush(stdout);
        if (scanf("%d", &guess) != 1) { printf("\nbye\n"); return 1; }
        tries++;
        if (guess < secret) printf("Higher!\n");
        else if (guess > secret) printf("Lower!\n");
    }
    printf("Correct, it was %d. You needed %d tries.\n", secret, tries);
    return 0;
}
