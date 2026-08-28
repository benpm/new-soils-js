# Future
*Miscellaneous ideas that may or may not make it into the game, these are all up for debate and discussion. Nothing here is ready for implementation*

## Items
- **Rope** - Upon placing onto another rope, extends the chain of Rope Blocks downward. Can be climbed up. Breaking one breaks all beneath. 

## Block Updates
To simulate the world efficiently, the concept of tick-based updating for slow things like growing a tree would be inefficient. Instead, those kinds of slow random processes are actually just generated once. Generating 1000s of random numbers when a single growth event could just be planned once and waited on. This also allows for time-skipping, which effectively simulates some processes all at once.